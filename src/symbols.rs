//! The interface's icons: SVG from the Solar set (solar_map.rs), drawn by Maia's own SVG
//! renderer at exactly the pixels they take on screen, as white masks the caller tints with
//! the theme's colour. Each is rendered once per size and kept; drawing one is a textured
//! square snapped to the pixel grid, so strokes stay sharp at every font size and scale.

use std::collections::HashMap;

use egui::{Color32, ColorImage, Id, Painter, Rect, TextureHandle, TextureId, Vec2, pos2};

/// The icons' textures by (place in the table, pixels a side).
type Cache = HashMap<(usize, u32), TextureHandle>;

fn find(name: &str) -> Option<usize> {
    crate::solar_map::SOLAR.binary_search_by(|(n, _)| (*n).cmp(name)).ok()
}

fn texture(ctx: &egui::Context, k: usize, px: u32) -> Option<TextureId> {
    let id = Id::new("solar icons");
    if let Some(t) = ctx.data(|d| d.get_temp::<Cache>(id).and_then(|c| c.get(&(k, px)).map(|t| t.id()))) {
        return Some(t);
    }
    // Made outside the data lock: loading a texture takes the context's own lock.
    let (w, h, rgba) = crate::svg::render(crate::solar_map::SOLAR[k].1.as_bytes(), px)?;
    let t = ctx.load_texture(format!("icon {} {px}", crate::solar_map::SOLAR[k].0), ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba), egui::TextureOptions::LINEAR);
    let tid = t.id();
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Cache>(id).insert((k, px), t));
    Some(tid)
}

/// Draw icon `name` centred in `rect`, its drawing spanning the rect's shorter side (the set
/// draws in the middle 20 units of a 24-unit square). False when there is no such icon.
pub fn paint(p: &Painter, rect: Rect, name: &str, col: Color32) -> bool {
    draw(p, rect, name, col, false)
}

/// `paint`, mirrored left to right: a turn to the right drawn as one to the left.
pub fn paint_mirrored(p: &Painter, rect: Rect, name: &str, col: Color32) -> bool {
    draw(p, rect, name, col, true)
}

fn draw(p: &Painter, rect: Rect, name: &str, col: Color32, mirror: bool) -> bool {
    let Some(k) = find(name) else { return false };
    let ppp = p.ctx().pixels_per_point();
    let s = rect.width().min(rect.height());
    let px = (s * 24.0 / 20.0 * ppp).round().max(8.0) as u32;
    let Some(tex) = texture(p.ctx(), k, px) else { return false };
    let side = px as f32 / ppp;
    // On whole pixels, or every edge would land between two and blur.
    let c = rect.center();
    let min = pos2(((c.x * ppp - px as f32 / 2.0).round()) / ppp, ((c.y * ppp - px as f32 / 2.0).round()) / ppp);
    let uv = if mirror { Rect::from_min_max(pos2(1.0, 0.0), pos2(0.0, 1.0)) } else { Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)) };
    p.image(tex, Rect::from_min_size(min, Vec2::splat(side)), uv, col);
    true
}
