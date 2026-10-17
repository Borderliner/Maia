//! Format a removable drive: the filesystem and a label, and the drive's name typed to
//! say yes, since everything on it is erased. The work itself is `drives::format`.

use egui::{Color32, Id, vec2};

use crate::app::App;
use crate::drives::{Drive, Fs};
use crate::popups::{self as pp, Popup};

pub struct State {
    pub drive: Drive,
    /// The whole disk's size, which is what is erased.
    pub size: u64,
    pub fs: Fs,
    pub label: String,
    /// The drive's name, typed to confirm.
    pub confirm: String,
    focus: bool,
}

pub fn open(drive: Drive) -> Popup {
    let size = crate::sidebar::drives().iter().find(|x| x.dev == drive.disk).map_or(drive.size, |x| x.size.max(drive.size));
    // The label it has now, as the starting point.
    let label = drive.label.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')).take(11).collect();
    Popup::FormatDrive(Box::new(State { drive, size, fs: Fs::Exfat, label, confirm: String::new(), focus: true }))
}

impl State {
    /// What has to be typed: the name the drive goes by in the sidebar.
    pub fn name(&self) -> String {
        self.drive.title()
    }

    pub fn confirmed(&self) -> bool {
        self.confirm.trim() == self.name()
    }
}

fn fs_tip(fs: Fs) -> &'static str {
    match fs {
        Fs::Fat32 => "Read by almost everything; no file over 4 GB",
        Fs::Exfat => "Big files; Windows, macOS, Linux, cameras and TVs",
        Fs::Ext4 => "Linux only, with its permissions",
    }
}

pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    let label = crate::drives::label_for(st.fs, &st.label);
    let ok = st.confirmed() && label.is_ok() && !crate::drives::busy(&st.drive.dev);
    let mut go = k.enter && ok;
    let mut cancel = false;
    let name = st.name();
    pp::centered(ctx, "format drive", 460.0, &pal, |ui| {
        pp::dialog_title(ui, &format!("Format “{name}”"), Some(&format!("{} removable drive · {}", crate::fsx::fmt_size(st.size), st.drive.disk)), &pal);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for f in Fs::ALL {
                let what = match f {
                    Fs::Fat32 => "format fat32",
                    Fs::Exfat => "format exfat",
                    Fs::Ext4 => "format ext4",
                };
                if crate::devtools::chip(ui, f.name(), st.fs == f, what, fs_tip(f), &pal, false) {
                    st.fs = f;
                }
            }
        });
        ui.label(egui::RichText::new(fs_tip(st.fs)).small().color(pal.text_dim));
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Label").small().color(pal.text_dim));
        let r = ui.add(egui::TextEdit::singleline(&mut st.label).id(Id::new("format label")).desired_width(f32::INFINITY));
        crate::ui::note_control(ui, r.rect, "format label");
        if let Err(why) = &label {
            ui.label(egui::RichText::new(why).small().color(pal.danger));
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new(format!("Everything on this {} drive is erased: every partition and every file on it.", crate::fsx::fmt_size(st.size))).color(pal.danger));
        ui.add_space(6.0);
        ui.label(egui::RichText::new(format!("Type {name} to confirm")).small().color(pal.text_dim));
        let r = ui.add(egui::TextEdit::singleline(&mut st.confirm).id(Id::new("format confirm")).hint_text(egui::RichText::new(&name).color(pal.text_dim.gamma_multiply(0.6))).desired_width(f32::INFINITY));
        crate::ui::note_control(ui, r.rect, "format confirm");
        if st.focus {
            r.request_focus();
            st.focus = false;
        }
        pp::dialog_buttons(ui, |ui| {
            let b = ui.add_enabled(ok, egui::Button::new(egui::RichText::new("Erase and Format").color(Color32::WHITE)).fill(pal.danger).min_size(vec2(72.0, 0.0)));
            crate::ui::note_control(ui, b.rect, "format erase");
            go |= b.clicked() && ok;
            let c = pp::plain_button(ui, "Cancel", &pal);
            crate::ui::note_control(ui, c.rect, "format cancel");
            cancel = c.clicked();
        });
    });
    if cancel {
        return false;
    }
    if go {
        if let Ok(l) = label {
            app.format_drive(st.drive.clone(), st.fs, l);
        }
        return false;
    }
    true
}
