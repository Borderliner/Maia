//! Colors, fonts and vector icons.

use egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeName {
    #[default]
    Midnight,
    Graphite,
    Lagoon,
    #[serde(alias = "Nord")]
    Navy,
    #[serde(alias = "Light")]
    Paper,
    Linen,
}

impl ThemeName {
    pub const ALL: [ThemeName; 6] = [ThemeName::Midnight, ThemeName::Graphite, ThemeName::Lagoon, ThemeName::Navy, ThemeName::Paper, ThemeName::Linen];
    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Midnight => "Midnight",
            ThemeName::Graphite => "Graphite",
            ThemeName::Lagoon => "Lagoon",
            ThemeName::Navy => "Navy",
            ThemeName::Paper => "Paper",
            ThemeName::Linen => "Linen",
        }
    }
    pub fn is_light(self) -> bool {
        matches!(self, ThemeName::Paper | ThemeName::Linen)
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub sidebar: Color32,
    pub raised: Color32,
    pub hover: Color32,
    pub selected: Color32,
    pub cursor: Color32,
    pub accent: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub border: Color32,
    pub danger: Color32,
    /// Title strip behind the tabs (the active tab takes the content color).
    pub chrome: Color32,
    /// Alternate (striped) rows.
    pub alt: Color32,
    /// Hovered list rows: a tint of the selection color.
    pub row_hover: Color32,
}

pub fn palette(t: ThemeName) -> Palette {
    let mut p = base_palette(t);
    let mix = |a: Color32, b: Color32, t: f32| Color32::from_rgb((a.r() as f32 + (b.r() as f32 - a.r() as f32) * t) as u8, (a.g() as f32 + (b.g() as f32 - a.g() as f32) * t) as u8, (a.b() as f32 + (b.b() as f32 - a.b() as f32) * t) as u8);
    if t == ThemeName::Midnight {
        // Neutral graphite: a lighter title strip, the sidebar a step above the list,
        // clearly visible stripes and a saturated teal selection.
        p.chrome = Color32::from_rgb(0x26, 0x27, 0x2a);
        p.alt = Color32::from_rgb(0x1d, 0x1e, 0x21);
    } else {
        p.chrome = mix(p.sidebar, p.text, if p.dark { 0.07 } else { 0.05 });
        p.alt = mix(p.panel, p.text, if p.dark { 0.035 } else { 0.03 });
    }
    p.row_hover = mix(p.panel, p.selected, if p.dark { 0.35 } else { 0.22 });
    p
}

fn base_palette(t: ThemeName) -> Palette {
    let c = Color32::from_rgb;
    let z = Color32::TRANSPARENT;
    match t {
        ThemeName::Midnight => Palette {
            dark: true,
            bg: c(0x18, 0x19, 0x1c),
            panel: c(0x15, 0x16, 0x19),
            sidebar: c(0x1d, 0x1e, 0x21),
            raised: c(0x25, 0x26, 0x2a),
            hover: c(0x27, 0x29, 0x2d),
            selected: c(0x12, 0x79, 0xa0),
            cursor: c(0x10, 0x88, 0xba),
            accent: c(0x1a, 0x91, 0xc8),
            text: c(0xde, 0xdf, 0xe1),
            text_dim: c(0x8c, 0x8f, 0x95),
            border: c(0x2c, 0x2d, 0x31),
            danger: c(0xff, 0x6b, 0x6b),
            chrome: z,
            alt: z,
            row_hover: z,
        },
        ThemeName::Graphite => Palette {
            dark: true,
            bg: c(0x1e, 0x1e, 0x1e),
            panel: c(0x25, 0x25, 0x26),
            sidebar: c(0x19, 0x19, 0x19),
            raised: c(0x2f, 0x2f, 0x31),
            hover: c(0x33, 0x33, 0x36),
            selected: c(0x3a, 0x3d, 0x41),
            cursor: c(0x4a, 0x4e, 0x55),
            accent: c(0xf0, 0xa0, 0x40),
            text: c(0xe8, 0xe8, 0xe8),
            text_dim: c(0x95, 0x95, 0x98),
            border: c(0x34, 0x34, 0x36),
            danger: c(0xff, 0x6b, 0x6b),
            chrome: z,
            alt: z,
            row_hover: z,
        },
        ThemeName::Lagoon => Palette {
            dark: true,
            bg: c(0x11, 0x1b, 0x1d),
            panel: c(0x16, 0x22, 0x25),
            sidebar: c(0x0e, 0x17, 0x19),
            raised: c(0x1e, 0x2d, 0x31),
            hover: c(0x21, 0x33, 0x37),
            selected: c(0x17, 0x4a, 0x4f),
            cursor: c(0x1d, 0x5e, 0x64),
            accent: c(0x3e, 0xd0, 0xc8),
            text: c(0xe2, 0xec, 0xec),
            text_dim: c(0x86, 0x9c, 0x9e),
            border: c(0x25, 0x35, 0x39),
            danger: c(0xff, 0x6b, 0x6b),
            chrome: z,
            alt: z,
            row_hover: z,
        },
        ThemeName::Navy => Palette {
            dark: true,
            bg: c(0x10, 0x16, 0x26),
            panel: c(0x15, 0x1c, 0x30),
            sidebar: c(0x0d, 0x12, 0x20),
            raised: c(0x1d, 0x26, 0x3e),
            hover: c(0x22, 0x2c, 0x47),
            selected: c(0x25, 0x3a, 0x6e),
            cursor: c(0x2e, 0x48, 0x88),
            accent: c(0x7a, 0xa2, 0xff),
            text: c(0xe3, 0xe8, 0xf4),
            text_dim: c(0x8a, 0x94, 0xb0),
            border: c(0x25, 0x2f, 0x4a),
            danger: c(0xff, 0x6b, 0x6b),
            chrome: z,
            alt: z,
            row_hover: z,
        },
        ThemeName::Paper => Palette {
            dark: false,
            bg: c(0xf7, 0xf8, 0xfa),
            panel: c(0xff, 0xff, 0xff),
            sidebar: c(0xee, 0xf0, 0xf3),
            raised: c(0xe8, 0xeb, 0xef),
            hover: c(0xe9, 0xee, 0xf6),
            selected: c(0xd4, 0xe4, 0xfb),
            cursor: c(0xbf, 0xd6, 0xf8),
            accent: c(0x1f, 0x6f, 0xeb),
            text: c(0x1c, 0x1f, 0x24),
            text_dim: c(0x6a, 0x72, 0x7f),
            border: c(0xdc, 0xe0, 0xe6),
            danger: c(0xd6, 0x33, 0x33),
            chrome: z,
            alt: z,
            row_hover: z,
        },
        ThemeName::Linen => Palette {
            dark: false,
            bg: c(0xf6, 0xf1, 0xe7),
            panel: c(0xfb, 0xf7, 0xef),
            sidebar: c(0xee, 0xe7, 0xd9),
            raised: c(0xe9, 0xe1, 0xd1),
            hover: c(0xec, 0xe3, 0xd2),
            selected: c(0xd9, 0xe7, 0xf2),
            cursor: c(0xc3, 0xda, 0xee),
            accent: c(0x1b, 0x7a, 0xb3),
            text: c(0x2b, 0x26, 0x20),
            text_dim: c(0x76, 0x6c, 0x5f),
            border: c(0xdd, 0xd3, 0xc1),
            danger: c(0xc8, 0x32, 0x32),
            chrome: z,
            alt: z,
            row_hover: z,
        },
    }
}

pub fn apply(ctx: &egui::Context, p: &Palette, font_size: f32, spacing: f32, animations: bool) {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.panel;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.raised;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.selected;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(6);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    // Slider rails, checkboxes and text fields sit on raised popups; keep them darker.
    v.widgets.inactive.bg_fill = p.bg;
    v.slider_trailing_fill = true;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.hovered.bg_fill = p.hover;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.weak_bg_fill = p.selected;
    v.widgets.active.bg_fill = p.selected;
    v.widgets.open.weak_bg_fill = p.hover;
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        use egui::{FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(font_size - 2.5)),
            (TextStyle::Body, FontId::proportional(font_size)),
            (TextStyle::Button, FontId::proportional(font_size)),
            (TextStyle::Heading, FontId::proportional(font_size + 5.0)),
            (TextStyle::Monospace, FontId::monospace(font_size - 1.0)),
        ]
        .into();
        // Whole points: a fractional gap (9.6 at Spacing 1.6) puts every widget after it
        // between pixels, softening edges.
        s.spacing.item_spacing = vec2((6.0 * spacing).round(), (4.0 * spacing).round());
        s.spacing.button_padding = vec2((8.0 * spacing).round(), (3.0 * spacing).round());
        s.spacing.interact_size.y = (20.0 * spacing).round();
        s.animation_time = if animations { 0.12 } else { 0.0 };
        s.interaction.selectable_labels = false;
    });
}

/// Fonts come from the system (so text matches the desktop); a tiny embedded
/// Noto subset (OFL) is the last resort so the UI always renders.
pub fn install_fonts(ctx: &egui::Context, family: &str, mono_family: &str) {
    const SANS: &[u8] = include_bytes!("../assets/fallback-sans.ttf");
    const MONO: &[u8] = include_bytes!("../assets/fallback-mono.ttf");
    const SYMBOLS: &[u8] = include_bytes!("../assets/fallback-symbols.ttf");
    // System fonts are memory-mapped, not read: egui then uses the bytes in place (no
    // copy) and the kernel only pages in the glyph tables actually used, sharing them
    // with every other app (the CJK fallback alone is ~20 MB).
    let first = |paths: &[&str]| paths.iter().find_map(|p| map_font(std::path::Path::new(p)));
    let fc_match = |family: &str| -> Option<&'static [u8]> {
        let out = std::process::Command::new("fc-match").args(["--format=%{file}", family]).output().ok()?;
        map_font(std::path::Path::new(String::from_utf8_lossy(&out.stdout).trim()))
    };
    let mut fonts = FontDefinitions::empty();
    let mut prop: Vec<String> = vec![];
    let mut mono: Vec<String> = vec![];
    let mut add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8], list: &mut Vec<String>| {
        fonts.font_data.insert(name.into(), FontData::from_static(bytes).into());
        list.push(name.into());
    };
    let custom = if family.trim().is_empty() { None } else { fc_match(family.trim()) };
    // Follow the desktop's UI font: the GTK font setting, then
    // fontconfig's system-ui alias, then a list of common sans families.
    let desktop = desktop_font_family().and_then(|f| fc_match(&f)).or_else(|| fc_match("system-ui"));
    let ui = custom.or(desktop).or_else(|| first(&[
        "/usr/share/fonts/inter/InterVariable.ttf",
        "/usr/share/fonts/inter/Inter-Regular.ttf",
        "/usr/share/fonts/TTF/Inter-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/cantarell/Cantarell-VF.otf",
    ]))
    .or_else(|| fc_match("sans-serif"));
    if let Some(b) = ui {
        add(&mut fonts, "ui", b, &mut prop);
    }
    // Symbols (arrows, stars, blocks) from the system when available.
    if let Some(b) = first(&["/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf", "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]) {
        add(&mut fonts, "symbols", b, &mut prop);
    }
    let custom_mono = if mono_family.trim().is_empty() { None } else { fc_match(mono_family.trim()) };
    let sys_mono = custom_mono.or_else(|| first(&[
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/Hack-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    ]))
    .or_else(|| fc_match("monospace"));
    if let Some(b) = sys_mono {
        add(&mut fonts, "mono", b, &mut mono);
        prop.push("mono".into()); // box-drawing and arrows for proportional text too
    }
    for p in ["/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"] {
        if let Some(b) = map_font(std::path::Path::new(p)) {
            add(&mut fonts, "cjk", b, &mut prop);
            break;
        }
    }
    // Interface glyphs: a Lucide subset (ISC), drawn as text in its own family.
    const ICONS: &[u8] = include_bytes!("../assets/lucide-subset.ttf");
    fonts.font_data.insert("lucide".into(), FontData::from_static(ICONS).into());
    fonts.families.insert(FontFamily::Name("icons".into()), vec!["lucide".into()]);
    for (name, bytes) in [("fb-sans", SANS), ("fb-mono", MONO), ("fb-symbols", SYMBOLS)] {
        fonts.font_data.insert(name.into(), FontData::from_static(bytes).into());
    }
    prop.extend(["fb-sans", "fb-symbols", "fb-mono"].map(String::from));
    mono.push("fb-mono".into());
    mono.extend(prop.iter().filter(|n| *n != "mono").cloned());
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Monospace, mono);
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------- icons

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileCat {
    Folder,
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Doc,
    Text,
    Sheet,
    Slides,
    Pdf,
    Font,
    Disk,
    Book,
    Exec,
    Other,
}

pub fn categorize(ext: &str, is_dir: bool, exec: bool) -> FileCat {
    if is_dir {
        return FileCat::Folder;
    }
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff" | "heic" | "avif" | "psd" | "xcf" => FileCat::Image,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "m4v" => FileCat::Video,
        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac" | "wma" => FileCat::Audio,
        "zip" | "tar" | "gz" | "xz" | "zst" | "bz2" | "7z" | "rar" | "tgz" | "txz" | "lz" | "lzma" | "cab" | "deb" | "rpm" | "pkg" | "apk" | "jar" | "whl" | "crate" => FileCat::Archive,
        "iso" | "img" | "dmg" | "qcow2" | "vmdk" | "vdi" | "vhd" | "vhdx" | "appimage" | "flatpak" | "snap" => FileCat::Disk,
        "rs" | "c" | "h" | "cpp" | "cc" | "hpp" | "py" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "go" | "java" | "kt" | "rb" | "php" | "sh" | "bash" | "fish" | "zsh" | "lua" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "htm" | "css" | "scss" | "sql" | "zig" | "cs" | "swift" | "conf" | "ini" | "cfg" | "vue" | "svelte" | "dart" | "hs" | "ex" | "nix" | "cmake" | "mk" | "gradle" | "diff" | "patch" => FileCat::Code,
        "txt" | "md" | "markdown" | "log" | "nfo" | "rst" | "org" | "srt" | "vtt" => FileCat::Text,
        "doc" | "docx" | "odt" | "rtf" | "tex" | "pages" => FileCat::Doc,
        "xls" | "xlsx" | "ods" | "csv" | "tsv" | "numbers" => FileCat::Sheet,
        "ppt" | "pptx" | "odp" | "key" => FileCat::Slides,
        "epub" | "mobi" | "azw3" | "djvu" | "fb2" | "cbz" | "cbr" => FileCat::Book,
        "ttf" | "otf" | "woff" | "woff2" | "ttc" | "pfb" => FileCat::Font,
        "pdf" => FileCat::Pdf,
        _ if exec => FileCat::Exec,
        _ => FileCat::Other,
    }
}

pub fn cat_color(c: FileCat) -> Color32 {
    let c32 = Color32::from_rgb;
    match c {
        FileCat::Folder => c32(0xf2, 0xc0, 0x4f),
        FileCat::Image => c32(0x4f, 0xc3, 0x8a),
        FileCat::Video => c32(0xe0, 0x5f, 0x8a),
        FileCat::Audio => c32(0xb0, 0x7c, 0xf0),
        FileCat::Archive => c32(0xd9, 0x8a, 0x4a),
        FileCat::Code => c32(0x58, 0xb4, 0xe8),
        FileCat::Doc => c32(0x4a, 0x86, 0xe8),
        FileCat::Text => c32(0xa8, 0xb2, 0xc2),
        FileCat::Sheet => c32(0x3f, 0xb3, 0x6b),
        FileCat::Slides => c32(0xf0, 0x8a, 0x3c),
        FileCat::Font => c32(0xd0, 0x6d, 0xc8),
        FileCat::Disk => c32(0x7f, 0x8f, 0xa8),
        FileCat::Book => c32(0xc9, 0x7b, 0x52),
        FileCat::Pdf => c32(0xe8, 0x5a, 0x4f),
        FileCat::Exec => c32(0x7c, 0xd6, 0x5a),
        FileCat::Other => c32(0x8a, 0x93, 0xa3),
    }
}

/// Draw a folder or document glyph centered in `rect`.
pub fn paint_icon(painter: &Painter, rect: Rect, cat: FileCat, ext: &str, is_link: bool) {
    let s = rect.width().min(rect.height());
    let c = rect.center();
    let col = cat_color(cat);
    if cat == FileCat::Folder {
        let w = s * 0.92;
        let h = s * 0.72;
        let r = Rect::from_center_size(c + vec2(0.0, s * 0.04), vec2(w, h));
        let tab = Rect::from_min_size(r.min - vec2(0.0, s * 0.08), vec2(w * 0.42, s * 0.2));
        let rad = CornerRadius::same((s * 0.08).max(1.0) as u8);
        painter.rect_filled(tab, rad, col.gamma_multiply(0.75));
        painter.rect_filled(r, rad, col);
        let front = Rect::from_min_max(pos2(r.min.x, r.min.y + h * 0.22), r.max);
        painter.rect_filled(front, rad, lighten(col, 0.12));
    } else {
        let w = s * 0.72;
        let h = s * 0.92;
        let r = Rect::from_center_size(c, vec2(w, h));
        let fold = s * 0.24;
        let pts = vec![
            r.left_top(),
            pos2(r.right() - fold, r.top()),
            pos2(r.right(), r.top() + fold),
            r.right_bottom(),
            r.left_bottom(),
        ];
        painter.add(Shape::convex_polygon(pts, col.gamma_multiply(0.9), Stroke::NONE));
        painter.add(Shape::convex_polygon(
            vec![pos2(r.right() - fold, r.top()), pos2(r.right(), r.top() + fold), pos2(r.right() - fold, r.top() + fold)],
            lighten(col, 0.35),
            Stroke::NONE,
        ));
        // A symbol for the kind of file, drawn on the page.
        if s >= 20.0 {
            let k = s * 0.22;
            let m = pos2(c.x, r.top() + h * (if s >= 28.0 && !ext.is_empty() { 0.42 } else { 0.55 }));
            let ink = Color32::from_rgba_unmultiplied(255, 255, 255, 215);
            let st = Stroke::new((s / 22.0).max(1.0), ink);
            let pt = |x: f32, y: f32| m + vec2(x * k, y * k);
            match cat {
                FileCat::Image => {
                    painter.add(Shape::convex_polygon(vec![pt(-1.1, 0.8), pt(-0.2, -0.4), pt(0.4, 0.3), pt(0.7, 0.0), pt(1.1, 0.8)], ink, Stroke::NONE));
                    painter.circle_filled(pt(0.6, -0.6), k * 0.25, ink);
                }
                FileCat::Video => {
                    painter.add(Shape::convex_polygon(vec![pt(-0.6, -0.8), pt(0.9, 0.0), pt(-0.6, 0.8)], ink, Stroke::NONE));
                }
                FileCat::Audio => {
                    painter.line_segment([pt(-0.3, 0.6), pt(-0.3, -0.8)], st);
                    painter.line_segment([pt(-0.3, -0.8), pt(0.7, -1.0)], st);
                    painter.line_segment([pt(0.7, -1.0), pt(0.7, 0.4)], st);
                    painter.circle_filled(pt(-0.55, 0.65), k * 0.32, ink);
                    painter.circle_filled(pt(0.45, 0.45), k * 0.32, ink);
                }
                FileCat::Archive => {
                    for i in 0..4 {
                        let y = -1.0 + i as f32 * 0.45;
                        painter.line_segment([pt(-0.25, y), pt(0.25, y)], st);
                    }
                    painter.rect_stroke(Rect::from_min_max(pt(-0.35, 0.8), pt(0.35, 1.3)), CornerRadius::same(1), st, StrokeKind::Middle);
                }
                FileCat::Code => {
                    painter.add(Shape::line(vec![pt(-0.4, -0.6), pt(-1.0, 0.0), pt(-0.4, 0.6)], st));
                    painter.add(Shape::line(vec![pt(0.4, -0.6), pt(1.0, 0.0), pt(0.4, 0.6)], st));
                    painter.line_segment([pt(0.2, -0.8), pt(-0.2, 0.8)], st);
                }
                FileCat::Text | FileCat::Doc => {
                    for (i, w) in [1.0f32, 0.7, 1.0, 0.55].iter().enumerate() {
                        let y = -0.75 + i as f32 * 0.5;
                        painter.line_segment([pt(-0.9, y), pt(-0.9 + 1.8 * w, y)], st);
                    }
                }
                FileCat::Sheet => {
                    painter.rect_stroke(Rect::from_min_max(pt(-0.9, -0.8), pt(0.9, 0.8)), CornerRadius::same(1), st, StrokeKind::Middle);
                    painter.line_segment([pt(-0.9, 0.0), pt(0.9, 0.0)], st);
                    painter.line_segment([pt(-0.2, -0.8), pt(-0.2, 0.8)], st);
                }
                FileCat::Slides => {
                    for (i, hgt) in [0.6f32, 1.2, 0.9].iter().enumerate() {
                        let x = -0.75 + i as f32 * 0.6;
                        painter.rect_filled(Rect::from_min_max(pt(x, 0.8 - hgt), pt(x + 0.35, 0.8)), CornerRadius::ZERO, ink);
                    }
                }
                FileCat::Pdf => {
                    painter.add(Shape::closed_line(vec![pt(0.0, -1.0), pt(0.9, 0.7), pt(-0.9, 0.7)], st));
                }
                FileCat::Font => {
                    painter.text(m, egui::Align2::CENTER_CENTER, "Aa", egui::FontId::proportional(k * 1.9), ink);
                }
                FileCat::Disk => {
                    painter.circle_stroke(m, k * 1.0, st);
                    painter.circle_filled(m, k * 0.25, ink);
                }
                FileCat::Book => {
                    painter.add(Shape::closed_line(vec![pt(-1.0, -0.7), pt(0.0, -0.45), pt(1.0, -0.7), pt(1.0, 0.8), pt(0.0, 1.0), pt(-1.0, 0.8)], st));
                    painter.line_segment([pt(0.0, -0.45), pt(0.0, 1.0)], st);
                }
                FileCat::Exec => {
                    painter.add(Shape::line(vec![pt(-0.9, -0.6), pt(-0.2, 0.0), pt(-0.9, 0.6)], st));
                    painter.line_segment([pt(0.0, 0.7), pt(0.9, 0.7)], st);
                }
                _ => {}
            }
        }
        if s >= 28.0 && !ext.is_empty() {
            let label: String = ext.chars().take(4).collect::<String>().to_uppercase();
            painter.text(
                pos2(c.x, r.bottom() - s * 0.17),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional((s * 0.18).clamp(7.0, 20.0)),
                Color32::from_rgba_unmultiplied(0, 0, 0, 160),
            );
        }
    }
    if is_link {
        let a = Rect::from_min_size(pos2(rect.left(), rect.bottom() - s * 0.38), Vec2::splat(s * 0.38));
        painter.rect(a, CornerRadius::same(2), Color32::WHITE, Stroke::new(1.0, Color32::DARK_GRAY), StrokeKind::Inside);
        painter.line_segment([a.left_bottom() + vec2(s * 0.08, -s * 0.08), a.right_top() + vec2(-s * 0.08, s * 0.08)], Stroke::new(1.5, Color32::BLACK));
    }
}

fn lighten(c: Color32, t: f32) -> Color32 {
    let f = |v: u8| (v as f32 + (255.0 - v as f32) * t) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// Small line-art glyphs for the sidebar & toolbar.
#[derive(Clone, Copy)]
pub enum Glyph {
    Home,
    Desktop,
    Docs,
    Down,
    Pics,
    Music,
    Video,
    Drive,
    Trash,
    Pin,
    Root,
    Back,
    Forward,
    Up,
    Refresh,
    Split,
    Search,
    Eye,
    Plus,
    Close,
    Clock,
    Bookmark,
    Folder,
    Monitor,
    CollapseLeft,
    Menu,
    ChevronRight,
    ChevronDown,
    Lock,
    LockOpen,
    Usb,
    Eject,
}

/// Draw a Lucide icon by name, centered in `rect` (the icon's stroke box roughly fills it).
pub fn paint_lucide(painter: &Painter, rect: Rect, name: &str, col: Color32) -> bool {
    let Some(&(_, ch)) = crate::lucide_map::LUCIDE.iter().find(|(n, _)| *n == name) else { return false };
    let s = rect.width().min(rect.height());
    // Lucide draws inside a 24-unit box with 2 units of padding: scale so the strokes span `s`.
    let size = (s * 24.0 / 20.0).round().max(6.0);
    let g = painter.layout_no_wrap(ch.to_string(), egui::FontId::new(size, FontFamily::Name("icons".into())), col);
    let pos = rect.center() - g.rect.size() / 2.0;
    painter.galley(pos2(pos.x.round(), pos.y.round()), g, col);
    true
}

/// Lucide name for each interface glyph.
fn glyph_name(g: Glyph) -> &'static str {
    match g {
        Glyph::Home => "house",
        Glyph::Desktop => "monitor",
        Glyph::Docs => "file-text",
        Glyph::Down => "download",
        Glyph::Pics => "image",
        Glyph::Music => "music",
        Glyph::Video => "video",
        Glyph::Drive => "hard-drive",
        Glyph::Trash => "trash-2",
        Glyph::Pin => "pin",
        Glyph::Root => "slash",
        Glyph::Back => "arrow-left",
        Glyph::Forward => "arrow-right",
        Glyph::ChevronRight => "chevron-right",
        Glyph::ChevronDown => "chevron-down",
        Glyph::Lock => "lock",
        Glyph::LockOpen => "lock-open",
        Glyph::Up => "arrow-up",
        Glyph::Refresh => "rotate-ccw",
        Glyph::Split => "columns-2",
        Glyph::Search => "search",
        Glyph::Eye => "scan-eye",
        Glyph::Plus => "plus",
        Glyph::Close => "x",
        Glyph::Clock => "clock",
        Glyph::Bookmark => "bookmark",
        Glyph::Folder => "folder",
        Glyph::Monitor => "monitor",
        Glyph::CollapseLeft => "panel-left-close",
        Glyph::Menu => "menu",
        Glyph::Usb => "usb",
        Glyph::Eject => "", // not in Lucide: drawn below
    }
}

pub fn paint_glyph(painter: &Painter, rect: Rect, g: Glyph, col: Color32) {
    if paint_lucide(painter, rect, glyph_name(g), col) {
        return;
    }
    let s = rect.width().min(rect.height());
    let c = rect.center();
    let st = Stroke::new((s / 11.0).max(1.2), col);
    let p = |x: f32, y: f32| -> Pos2 { c + vec2(x * s, y * s) };
    let line = |a: Pos2, b: Pos2| painter.line_segment([a, b], st);
    match g {
        Glyph::Home => {
            painter.add(Shape::line(vec![p(-0.35, 0.0), p(0.0, -0.32), p(0.35, 0.0)], st));
            painter.add(Shape::line(vec![p(-0.25, -0.08), p(-0.25, 0.32), p(0.25, 0.32), p(0.25, -0.08)], st));
        }
        Glyph::Desktop => {
            painter.rect_stroke(Rect::from_min_max(p(-0.38, -0.3), p(0.38, 0.18)), CornerRadius::same(2), st, StrokeKind::Middle);
            line(p(0.0, 0.18), p(0.0, 0.32));
            line(p(-0.18, 0.32), p(0.18, 0.32));
        }
        Glyph::Docs => {
            painter.rect_stroke(Rect::from_min_max(p(-0.28, -0.36), p(0.28, 0.36)), CornerRadius::same(2), st, StrokeKind::Middle);
            for y in [-0.15, 0.0, 0.15] {
                line(p(-0.15, y), p(0.15, y));
            }
        }
        Glyph::Down => {
            line(p(0.0, -0.36), p(0.0, 0.16));
            painter.add(Shape::line(vec![p(-0.2, -0.04), p(0.0, 0.16), p(0.2, -0.04)], st));
            line(p(-0.32, 0.34), p(0.32, 0.34));
        }
        Glyph::Pics => {
            painter.rect_stroke(Rect::from_min_max(p(-0.38, -0.3), p(0.38, 0.3)), CornerRadius::same(2), st, StrokeKind::Middle);
            painter.add(Shape::line(vec![p(-0.38, 0.22), p(-0.1, -0.04), p(0.1, 0.14), p(0.2, 0.04), p(0.38, 0.2)], st));
            painter.circle_stroke(p(0.15, -0.14), s * 0.06, st);
        }
        Glyph::Music => {
            line(p(-0.12, 0.24), p(-0.12, -0.3));
            line(p(-0.12, -0.3), p(0.28, -0.36));
            line(p(0.28, -0.36), p(0.28, 0.18));
            painter.circle_filled(p(-0.2, 0.26), s * 0.1, col);
            painter.circle_filled(p(0.2, 0.2), s * 0.1, col);
        }
        Glyph::Video => {
            painter.rect_stroke(Rect::from_min_max(p(-0.38, -0.24), p(0.16, 0.24)), CornerRadius::same(2), st, StrokeKind::Middle);
            painter.add(Shape::line(vec![p(0.16, -0.06), p(0.38, -0.2), p(0.38, 0.2), p(0.16, 0.06)], st));
        }
        Glyph::Drive => {
            painter.rect_stroke(Rect::from_min_max(p(-0.38, -0.18), p(0.38, 0.22)), CornerRadius::same(3), st, StrokeKind::Middle);
            painter.circle_filled(p(0.22, 0.02), s * 0.05, col);
            line(p(-0.24, 0.02), p(0.06, 0.02));
        }
        Glyph::Trash => {
            line(p(-0.34, -0.24), p(0.34, -0.24));
            line(p(-0.1, -0.34), p(0.1, -0.34));
            painter.add(Shape::line(vec![p(-0.26, -0.24), p(-0.2, 0.36), p(0.2, 0.36), p(0.26, -0.24)], st));
        }
        Glyph::Pin => {
            painter.circle_stroke(p(0.0, -0.14), s * 0.18, st);
            line(p(0.0, 0.04), p(0.0, 0.38));
        }
        Glyph::Root => {
            line(p(-0.3, -0.3), p(0.3, 0.3));
        }
        Glyph::Back => {
            painter.add(Shape::line(vec![p(0.12, -0.3), p(-0.18, 0.0), p(0.12, 0.3)], st));
        }
        Glyph::Forward => {
            painter.add(Shape::line(vec![p(-0.12, -0.3), p(0.18, 0.0), p(-0.12, 0.3)], st));
        }
        Glyph::Up => {
            painter.add(Shape::line(vec![p(-0.3, 0.12), p(0.0, -0.18), p(0.3, 0.12)], st));
        }
        Glyph::Refresh => {
            let pts: Vec<Pos2> = (0..=20)
                .map(|i| {
                    let a = 0.6 + i as f32 / 20.0 * 5.0;
                    c + vec2(a.cos(), a.sin()) * s * 0.3
                })
                .collect();
            let end = *pts.last().unwrap();
            painter.add(Shape::line(pts, st));
            painter.add(Shape::line(vec![end + vec2(-s * 0.16, -s * 0.02), end, end + vec2(-s * 0.02, s * 0.16)], st));
        }
        Glyph::Split => {
            painter.rect_stroke(Rect::from_min_max(p(-0.38, -0.3), p(0.38, 0.3)), CornerRadius::same(2), st, StrokeKind::Middle);
            line(p(0.0, -0.3), p(0.0, 0.3));
        }
        Glyph::Search => {
            painter.circle_stroke(p(-0.06, -0.06), s * 0.24, st);
            line(p(0.12, 0.12), p(0.34, 0.34));
        }
        Glyph::Eye => {
            let top: Vec<Pos2> = (0..=16).map(|i| { let t = i as f32 / 16.0; p(-0.4 + 0.8 * t, -0.24 * (std::f32::consts::PI * t).sin()) }).collect();
            let bot: Vec<Pos2> = (0..=16).map(|i| { let t = i as f32 / 16.0; p(-0.4 + 0.8 * t, 0.24 * (std::f32::consts::PI * t).sin()) }).collect();
            painter.add(Shape::line(top, st));
            painter.add(Shape::line(bot, st));
            painter.circle_filled(c, s * 0.1, col);
        }
        Glyph::Plus => {
            line(p(-0.3, 0.0), p(0.3, 0.0));
            line(p(0.0, -0.3), p(0.0, 0.3));
        }
        Glyph::Close => {
            line(p(-0.25, -0.25), p(0.25, 0.25));
            line(p(-0.25, 0.25), p(0.25, -0.25));
        }
        Glyph::Bookmark => {
            painter.add(Shape::closed_line(vec![p(-0.24, -0.36), p(0.24, -0.36), p(0.24, 0.38), p(0.0, 0.16), p(-0.24, 0.38)], st));
        }
        Glyph::Folder => {
            painter.add(Shape::closed_line(vec![p(-0.38, -0.26), p(-0.1, -0.26), p(0.0, -0.16), p(0.38, -0.16), p(0.38, 0.3), p(-0.38, 0.3)], st));
        }
        Glyph::Monitor => {
            painter.rect_stroke(Rect::from_min_max(p(-0.4, -0.3), p(0.4, 0.18)), CornerRadius::same(2), st, StrokeKind::Middle);
            line(p(-0.16, 0.34), p(0.16, 0.34));
            line(p(0.0, 0.18), p(0.0, 0.34));
        }
        Glyph::CollapseLeft => {
            line(p(-0.34, -0.32), p(-0.34, 0.32));
            line(p(0.34, 0.0), p(-0.16, 0.0));
            painter.add(Shape::line(vec![p(0.0, -0.16), p(-0.16, 0.0), p(0.0, 0.16)], st));
        }
        Glyph::ChevronRight | Glyph::ChevronDown | Glyph::Lock | Glyph::LockOpen | Glyph::Usb => {}
        Glyph::Eject => {
            // Rounded triangle over a bar, in Lucide's stroke style.
            let tri = vec![p(0.0, -0.34), p(0.34, 0.06), p(-0.34, 0.06)];
            painter.add(Shape::Path(egui::epaint::PathShape { points: tri, closed: true, fill: Color32::TRANSPARENT, stroke: st.into() }));
            painter.rect_stroke(Rect::from_min_max(p(-0.34, 0.2), p(0.34, 0.32)), CornerRadius::same(1), st, StrokeKind::Middle);
        }
        Glyph::Menu => {
            for y in [-0.24, 0.0, 0.24] {
                line(p(-0.32, y), p(0.32, y));
            }
        }
        Glyph::Clock => {
            painter.circle_stroke(c, s * 0.34, st);
            painter.add(Shape::line(vec![p(0.0, -0.2), p(0.0, 0.0), p(0.14, 0.1)], st));
        }
    }
}

/// Family name of the desktop UI font (GNOME/GTK setting), without the size.
fn desktop_font_family() -> Option<String> {
    let raw = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "font-name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let cfg = dirs::config_dir()?;
            ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"].iter().find_map(|f| {
                std::fs::read_to_string(cfg.join(f)).ok()?.lines().find_map(|l| l.trim().strip_prefix("gtk-font-name=").map(|v| v.trim().trim_matches('"').to_string()))
            })
        })?;
    // "Google Sans Flex 11" -> "Google Sans Flex"
    let fam = raw.rsplit_once(' ').filter(|(_, size)| size.parse::<f32>().is_ok()).map(|(f, _)| f.to_string()).unwrap_or(raw);
    (!fam.is_empty()).then_some(fam)
}

/// Read-only shared mapping of a font file, kept for the life of the process (and reused
/// if the same file is requested again, e.g. after a font option change).
fn map_font(path: &std::path::Path) -> Option<&'static [u8]> {
    use std::os::fd::AsRawFd;
    static MAPS: std::sync::Mutex<Vec<(std::path::PathBuf, usize, usize)>> = std::sync::Mutex::new(Vec::new());
    let canon = std::fs::canonicalize(path).ok()?;
    let mut maps = MAPS.lock().ok()?;
    if let Some(&(_, ptr, len)) = maps.iter().find(|(p, ..)| *p == canon) {
        return Some(unsafe { std::slice::from_raw_parts(ptr as *const u8, len) });
    }
    let f = std::fs::File::open(&canon).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    if len < 12 {
        return None;
    }
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    maps.push((canon, ptr as usize, len));
    Some(unsafe { std::slice::from_raw_parts(ptr as *const u8, len) })
}
