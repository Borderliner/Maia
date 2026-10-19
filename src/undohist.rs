//! Undo History: the changes Undo can take back, newest first. Taking one back takes back
//! everything after it first, in order, the way pressing Undo that many times would:
//! a change undone out of order could put a file back where a later change expects it
//! gone.

use egui::{CornerRadius, FontId, Sense, pos2, vec2};

use crate::app::{App, UndoKind};
use crate::popups as pp;

pub struct State {
    /// The row chosen, 0 being the newest change.
    pub sel: usize,
}

/// A few words for what a change was, under its own label.
fn what(k: &UndoKind) -> &'static str {
    match k {
        UndoKind::Rename(_) => "Renamed",
        UndoKind::Move(_) => "Moved",
        UndoKind::Created(_) => "Made",
        UndoKind::Trashed(_) => "Moved to the trash",
        UndoKind::Replaced(_) => "Changed in place",
        UndoKind::Sorted(..) => "Sorted into folders",
        UndoKind::Copied(..) => "Copied, replacing",
        UndoKind::MovedOver(..) => "Moved, replacing",
        UndoKind::Synced(..) => "Synced",
    }
}

pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    let n = app.undo.len();
    pp::step(&mut st.sel, n, &k, |_| false);
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let mut take: Option<usize> = (k.enter && n > 0).then_some(st.sel);
    let mut close = false;
    let list_max = pp::fit_list(ctx, 420.0, 170.0);
    let (_, rect) = pp::centered(ctx, "undo history", 600.0, &pal, |ui| {
        pp::dialog_title(ui, "Undo history", Some("Taking a change back takes back the ones after it first."), &pal);
        ui.add_space(8.0);
        if n == 0 {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Nothing to undo yet").color(pal.text_dim));
            ui.add_space(6.0);
        } else {
            let row_h = (fs * 1.35 + small * 1.35 + 14.0).round();
            let mut area = egui::ScrollArea::vertical().id_salt("undo history list").max_height(list_max).min_scrolled_height((n as f32 * row_h).min(list_max)).auto_shrink([false, true]);
            if k.up || k.down || k.page_up || k.page_down {
                let most = (n as f32 * row_h - list_max).max(0.0);
                area = area.vertical_scroll_offset((st.sel as f32 * row_h - list_max / 2.0 + row_h / 2.0).clamp(0.0, most));
            }
            area.show_rows(ui, row_h, n, |ui, range| {
                for j in range {
                    // Row j is the change j steps back from the latest.
                    let i = n - 1 - j;
                    let e = &app.undo[i];
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    // Everything up to the chosen row goes with it: shown as one band.
                    let on = j == st.sel;
                    let going = j < st.sel;
                    if on {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                    } else if going {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected.gamma_multiply(0.45));
                    } else if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                    }
                    let (tc, dc) = if on { (pal.on_sel(), pal.on_sel_dim()) } else { (pal.text, pal.text_dim) };
                    let when = crate::devtools::ago(app.undo.when(i));
                    let wg = ui.painter().layout_no_wrap(when, FontId::proportional(small), dc);
                    let ww = wg.size().x;
                    ui.painter().galley(pos2(r.right() - 12.0 - ww, r.top() + 7.0 + (fs * 1.35 - wg.size().y) / 2.0), wg, dc);
                    let x = r.left() + 12.0;
                    let room = r.width() - 36.0 - ww;
                    let g = crate::listing::elided(ui, &e.label, FontId::proportional(fs), room, tc);
                    ui.painter().galley(pos2(x, r.top() + 6.0), g, tc);
                    let c = e.kind.count();
                    let sub = format!("{} · {c} {}", what(&e.kind), if c == 1 { "item" } else { "items" });
                    let g = crate::listing::elided(ui, &sub, FontId::proportional(small), room, dc);
                    ui.painter().galley(pos2(x, r.top() + 8.0 + fs * 1.35), g, dc);
                    crate::ui::note_control(ui, r.shrink(1.0), "undo row");
                    if resp.clicked() {
                        st.sel = j;
                    }
                    if resp.double_clicked() {
                        take = Some(j);
                    }
                }
            });
        }
        pp::hints(ui, &[("↑↓", "choose"), ("Enter", "take back"), ("Esc", "close")], &pal);
        pp::dialog_buttons(ui, |ui| {
            let label = match st.sel {
                _ if n == 0 => "Undo".to_string(),
                0 => "Undo".to_string(),
                s => format!("Undo {} changes", s + 1),
            };
            let b = pp::primary_button(ui, &label, n > 0, &pal);
            crate::ui::note_control(ui, b.rect, "undo back");
            if b.clicked() {
                take = Some(st.sel);
            }
            let c = pp::plain_button(ui, "Close", &pal);
            crate::ui::note_control(ui, c.rect, "undo close");
            close = c.clicked();
        });
    });
    if let Some(j) = take {
        let mut whole = true;
        for _ in 0..=j.min(n.saturating_sub(1)) {
            whole &= app.undo_last();
        }
        if j > 0 {
            app.toast(if whole { format!("Undid {} changes", j + 1) } else { format!("Undid {} changes; some files couldn't go back", j + 1) });
        }
        return false;
    }
    !(close || pp::clicked_outside(ctx, rect))
}
