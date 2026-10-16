//! SQLite databases through the `sqlite3` program, when it is installed: the tables and
//! views with their row counts, and the first rows of the one picked. Only Maia's own
//! fixed queries run, on a read-only, immutable connection in safe mode, so neither the
//! file nor anything around it can change and a view cannot load code.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use egui::{Align2, CornerRadius, FontId, Key, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

use crate::drives::Tools;
use crate::table::{Table, TableView};
use crate::theme::Palette;

/// Rows shown of a table.
pub const ROWS: usize = 500;
/// Tables whose rows are counted (each count is its own short query).
const COUNTED: usize = 200;

pub fn is_db_name(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".sqlite") || l.ends_with(".sqlite3") || l.ends_with(".db") || l.ends_with(".db3")
}

pub fn is_db_header(head: &[u8]) -> bool {
    head.starts_with(b"SQLite format 3\0")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub view: bool,
    pub rows: Option<u64>,
}

/// The database as a read-only, immutable URI: nothing is locked, no journal is made.
pub fn uri(path: &Path) -> String {
    let mut s = String::from("file:");
    for &b in path.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s.push_str("?mode=ro&immutable=1");
    s
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Safe mode (no extensions, no attaching, no file functions) came with 3.37.
pub fn parse_version(out: &str) -> Option<(u32, u32)> {
    let v = out.split_whitespace().next()?;
    let mut it = v.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn run(t: &Tools, path: &Path, safe: bool, sql: &str, header: bool, timeout: Duration) -> Option<String> {
    let mut c = crate::media::tool_cmd(t.cmd("sqlite3"));
    c.args(["-readonly", "-batch", "-bail", "-init", "/dev/null", "-csv", "-nullvalue", "NULL"]);
    if safe {
        c.arg("-safe");
    }
    c.arg(if header { "-header" } else { "-noheader" });
    c.arg(uri(path)).arg(sql);
    let out = crate::media::output_capped(c, timeout, 16 << 20, &|| false)?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// `type,name` rows of the schema listing.
pub fn parse_list(out: &str) -> Vec<Item> {
    crate::table::parse(out, b',', 100_000).0.into_iter().filter(|r| r.len() >= 2).map(|r| Item { view: r[0] == "view", name: r[1].clone(), rows: None }).collect()
}

const LIST: &str = "SELECT type, name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY type, name;";

/// The query for the first rows: blobs as their size, NULL as the word, everything else
/// as it is. NULL is spelt out here rather than with -nullvalue, which sqlite3 3.54 and
/// later ignore in CSV mode (an empty field there).
pub fn rows_query(name: &str, cols: &[String]) -> String {
    let sel: Vec<String> = cols.iter().map(|c| {
        let q = quote_ident(c);
        format!("CASE WHEN {q} IS NULL THEN 'NULL' WHEN typeof({q}) = 'blob' THEN '<' || length({q}) || ' bytes>' ELSE {q} END AS {q}")
    }).collect();
    let what = if sel.is_empty() { "*".to_string() } else { sel.join(", ") };
    format!("SELECT {what} FROM {} LIMIT {ROWS};", quote_ident(name))
}

pub enum Msg {
    /// The tables and views, or why there are none.
    List(Result<Vec<Item>, String>),
    Count(usize, u64),
    Rows(usize, Result<Table, String>),
}

/// The database's tables and counts, read on a worker.
fn list_and_count(t: Tools, path: PathBuf, tx: std::sync::mpsc::Sender<Msg>, ctx: egui::Context) {
    let (items, safe) = match tables(&t, &path) {
        Ok(x) => x,
        Err(e) => {
            let _ = tx.send(Msg::List(Err(e)));
            ctx.request_repaint();
            return;
        }
    };
    let names: Vec<(String, bool)> = items.iter().map(|i| (i.name.clone(), i.view)).collect();
    if tx.send(Msg::List(Ok(items))).is_err() {
        return;
    }
    ctx.request_repaint();
    for (k, (name, view)) in names.iter().enumerate().take(COUNTED) {
        let sql = format!("SELECT count(*) FROM {};", quote_ident(name));
        let limit = if *view { Duration::from_secs(3) } else { Duration::from_secs(8) };
        if let Some(n) = run(&t, &path, safe, &sql, false, limit).and_then(|o| o.trim().parse().ok()) {
            if tx.send(Msg::Count(k, n)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

/// The tables and views, and whether safe mode is on. Without it a view could run
/// anything, so only tables are listed then.
pub fn tables(t: &Tools, path: &Path) -> Result<(Vec<Item>, bool), String> {
    let safe = safe_mode(t);
    let mut items = run(t, path, safe, LIST, false, Duration::from_secs(10)).map(|o| parse_list(&o)).ok_or_else(|| "Can't read this database".to_string())?;
    if !safe {
        items.retain(|i| !i.view);
    }
    Ok((items, safe))
}

/// Whether this `sqlite3` has safe mode.
fn safe_mode(t: &Tools) -> bool {
    let mut c = crate::media::tool_cmd(t.cmd("sqlite3"));
    c.arg("-version");
    crate::media::output(c, Duration::from_secs(5)).map(|o| String::from_utf8_lossy(&o).into_owned()).and_then(|v| parse_version(&v)).is_some_and(|v| v >= (3, 37))
}

pub fn read_rows(t: &Tools, path: &Path, name: &str) -> Result<Table, String> {
    let safe = safe_mode(t);
    let cols_sql = format!("SELECT name FROM pragma_table_info({});", quote_str(name));
    let cols: Vec<String> = run(t, path, safe, &cols_sql, false, Duration::from_secs(5)).map(|o| crate::table::parse(&o, b',', 4000).0.into_iter().filter_map(|r| r.into_iter().next()).collect()).unwrap_or_default();
    let out = run(t, path, safe, &rows_query(name, &cols), true, Duration::from_secs(15)).ok_or_else(|| "Can't read this table".to_string())?;
    let (rows, _) = crate::table::parse(&out, b',', ROWS + 1);
    if rows.is_empty() {
        // No rows: the header alone, from the column names.
        return Ok(Table::new(vec![cols], false, Some(true)));
    }
    Ok(Table::new(rows, false, Some(true)))
}

pub struct View {
    pub path: PathBuf,
    tools: Tools,
    pub items: Option<Result<Vec<Item>, String>>,
    pub sel: usize,
    pub focus: bool,
    pub rows: Option<(usize, Result<TableView, String>)>,
    rx: Receiver<Msg>,
    rows_rx: Option<Receiver<Msg>>,
    reveal: bool,
}

impl View {
    pub fn start(path: PathBuf, tools: Tools, ctx: &egui::Context) -> View {
        let (tx, rx) = channel();
        let (t, p, c) = (tools.clone(), path.clone(), ctx.clone());
        std::thread::spawn(move || list_and_count(t, p, tx, c));
        View { path, tools, items: None, sel: 0, focus: false, rows: None, rx, rows_rx: None, reveal: false }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::List(l) => self.items = Some(l),
                Msg::Count(k, n) => {
                    if let Some(Ok(items)) = &mut self.items {
                        if let Some(i) = items.get_mut(k) {
                            i.rows = Some(n);
                        }
                    }
                }
                Msg::Rows(..) => {}
            }
        }
        if let Some(rx) = &self.rows_rx {
            if let Ok(Msg::Rows(k, r)) = rx.try_recv() {
                self.rows = Some((k, r.map(TableView::new)));
                self.rows_rx = None;
            }
        }
        // The picked table's rows, asked for once it changes.
        let want = match &self.items {
            Some(Ok(items)) if !items.is_empty() => Some(self.sel.min(items.len() - 1)),
            _ => None,
        };
        if let Some(k) = want {
            let have = self.rows.as_ref().map(|r| r.0);
            if have != Some(k) && self.rows_rx.is_none() {
                let Some(Ok(items)) = &self.items else { return };
                let (tx, rx) = channel();
                let (t, p, name, c) = (self.tools.clone(), self.path.clone(), items[k].name.clone(), ctx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(Msg::Rows(k, read_rows(&t, &p, &name)));
                    c.request_repaint();
                });
                self.rows_rx = Some(rx);
            }
        }
    }

    pub fn takes(&self, k: Key) -> bool {
        self.focus && matches!(k, Key::ArrowUp | Key::ArrowDown | Key::Home | Key::End | Key::Escape)
    }

    pub fn key(&mut self, k: Key) {
        let n = match &self.items {
            Some(Ok(i)) => i.len(),
            _ => 0,
        };
        match k {
            Key::ArrowDown => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            Key::ArrowUp => self.sel = self.sel.saturating_sub(1),
            Key::Home => self.sel = 0,
            Key::End => self.sel = n.saturating_sub(1),
            Key::Escape => self.focus = false,
            _ => {}
        }
        self.reveal = true;
    }

    /// (tables and views, counts read, rows shown of the picked one) for tests.
    #[cfg(test)]
    pub fn state(&self) -> (Option<usize>, usize, Option<(usize, usize)>) {
        let items = match &self.items {
            Some(Ok(i)) => Some(i.len()),
            _ => None,
        };
        let counted = match &self.items {
            Some(Ok(i)) => i.iter().filter(|x| x.rows.is_some()).count(),
            _ => 0,
        };
        (items, counted, self.rows.as_ref().and_then(|(k, r)| r.as_ref().ok().map(|t| (*k, t.t.rows.len()))))
    }
}

pub fn view(ui: &mut Ui, body: Rect, v: &mut View, size: f32, pal: &Palette, spacing: f32, id: u64) {
    v.poll(ui.ctx());
    let font = FontId::proportional(size);
    let msg = |ui: &Ui, s: &str| {
        ui.painter().text(body.center(), Align2::CENTER_CENTER, s, FontId::proportional(12.5), pal.text_dim);
    };
    let items = match &v.items {
        None => return msg(ui, "Reading the database…"),
        Some(Err(e)) => return msg(ui, e),
        Some(Ok(i)) if i.is_empty() => return msg(ui, "No tables"),
        Some(Ok(i)) => i,
    };
    v.sel = v.sel.min(items.len() - 1);
    let row_h = crate::inspector::row_height(ui, size, spacing).max((size + 8.0).round());
    // The list takes what it needs, up to two fifths of the preview; the rows the rest.
    let list_h = (items.len() as f32 * row_h + 8.0).min((body.height() * 0.4).max(row_h * 2.0 + 8.0));
    let list = Rect::from_min_max(body.min, pos2(body.right(), body.top() + list_h));
    let mut clicked = None;
    let reveal = std::mem::take(&mut v.reveal);
    let mut inner = ui.new_child(UiBuilder::new().max_rect(list.shrink2(vec2(6.0, 4.0)).translate(vec2(0.0, 2.0))));
    egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(("sqlite-list", id)).show_rows(&mut inner, row_h, items.len(), |ui, range| {
        for i in range {
            let it = &items[i];
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
            crate::ui::note_control(ui, r, "database table");
            let on = i == v.sel;
            if on {
                ui.painter().rect_filled(r, CornerRadius::same(4), if v.focus { pal.selected } else { pal.hover });
            } else if resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(4), pal.hover.gamma_multiply(0.6));
            }
            let (tc, dc) = if on && v.focus { (pal.on_sel(), pal.on_sel_dim()) } else { (pal.text, pal.text_dim) };
            let count = match it.rows {
                Some(1) => "1 row".to_string(),
                Some(n) => format!("{} rows", crate::table::fmt_count(n as usize)),
                None => String::new(),
            };
            let cg = ui.painter().layout_no_wrap(count, FontId::proportional(size - 1.0), dc);
            let tag = if it.view { "view" } else { "" };
            let tg = ui.painter().layout_no_wrap(tag.into(), FontId::proportional(size - 1.5), dc);
            let name_w = r.width() - cg.size().x - tg.size().x - 30.0;
            if name_w > 16.0 {
                let g = crate::listing::elided(ui, &it.name, font.clone(), name_w, tc);
                let gw = g.size().x;
                ui.painter().galley(pos2(r.left() + 8.0, r.center().y - g.size().y / 2.0), g, tc);
                ui.painter().galley(pos2(r.left() + 16.0 + gw, r.center().y - tg.size().y / 2.0), tg, dc);
            }
            ui.painter().galley(pos2(r.right() - 8.0 - cg.size().x, r.center().y - cg.size().y / 2.0), cg, dc);
            if resp.clicked() {
                clicked = Some(i);
            }
            if reveal && on {
                ui.scroll_to_rect(r, None);
            }
        }
    });
    if let Some(i) = clicked {
        v.sel = i;
        v.focus = true;
    }
    ui.painter().line_segment([pos2(body.left(), list.bottom()), pos2(body.right(), list.bottom())], Stroke::new(1.0, pal.border));
    let rest = Rect::from_min_max(pos2(body.left(), list.bottom() + 1.0), body.max);
    match &mut v.rows {
        Some((k, Ok(t))) if *k == v.sel => crate::table::view(ui, rest, t, size, pal, spacing, ("sqlite", id, *k)),
        Some((k, Err(e))) if *k == v.sel => {
            ui.painter().text(rest.center(), Align2::CENTER_CENTER, e.as_str(), FontId::proportional(12.5), pal.text_dim);
        }
        _ => {
            ui.painter().text(rest.center(), Align2::CENTER_CENTER, "Reading rows…", FontId::proportional(12.5), pal.text_dim);
        }
    }
    if ui.input(|i| i.pointer.any_pressed()) && ui.ctx().pointer_interact_pos().is_some_and(|pp| !list.contains(pp)) {
        v.focus = false;
    }
}
