//! Renders the tray icon as an ARGB32 pixmap. On macOS the menu bar showed an
//! SF Symbol plus colored text; Cinnamon's tray renders neither SNI title text
//! nor templated symbols, so PitStop bakes the percentage straight into the
//! icon image, color-coded by threshold, and dimmed when the data is stale.

use crate::model::IndicatorStyle;
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use std::sync::OnceLock;

pub struct IconImage {
    pub width: i32,
    pub height: i32,
    /// ARGB32, network byte order — bytes are [A, R, G, B] per pixel.
    pub argb: Vec<u8>,
}

const SIZE: i32 = 48;

static FONT: OnceLock<Option<FontVec>> = OnceLock::new();

fn font() -> Option<&'static FontVec> {
    FONT.get_or_init(|| {
        const PATHS: [&str; 6] = [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
        ];
        for p in PATHS {
            if let Ok(bytes) = std::fs::read(p) {
                if let Ok(f) = FontVec::try_from_vec(bytes) {
                    return Some(f);
                }
            }
        }
        None
    })
    .as_ref()
}

/// Threshold color: green < 75, orange ≥ 75, red ≥ 90, gray when no data.
fn color_for(pct: Option<i64>) -> [u8; 3] {
    match pct {
        Some(p) if p >= 90 => [0xE5, 0x3E, 0x3E],
        Some(p) if p >= 75 => [0xE8, 0x8A, 0x2E],
        Some(_) => [0x46, 0xB4, 0x50],
        None => [0x9A, 0x9A, 0x9A],
    }
}

pub fn placeholder() -> IconImage {
    render(None, false, IndicatorStyle::IconAndPercent)
}

pub fn render(pct: Option<i64>, stale: bool, style: IndicatorStyle) -> IconImage {
    let (w, h) = (SIZE, SIZE);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let color = color_for(pct);
    let alpha = if stale { 0.5 } else { 1.0 };
    let show_number = !matches!(style, IndicatorStyle::IconOnly);

    match (pct, show_number, font()) {
        (Some(p), true, Some(font)) => {
            let text = p.to_string();
            // Three digits ("100") need a smaller scale to fit.
            let px = if text.len() >= 3 { 30.0 } else { 40.0 };
            draw_text_centered(&mut buf, w, h, &text, px, color, alpha, font);
            if matches!(style, IndicatorStyle::IconAndPercent) {
                draw_checker_strip(&mut buf, w, h, color, alpha);
            }
        }
        _ => {
            // Icon-only, no data, or no font available: a checkered-flag motif.
            draw_flag(&mut buf, w, h, color, alpha);
        }
    }

    IconImage {
        width: w,
        height: h,
        argb: buf,
    }
}

fn draw_text_centered(
    buf: &mut [u8],
    img_w: i32,
    img_h: i32,
    text: &str,
    px: f32,
    color: [u8; 3],
    alpha: f32,
    font: &FontVec,
) {
    let scale = PxScale::from(px);
    let sf = font.as_scaled(scale);

    // Lay out glyphs along a baseline, collecting their outlines + overall bbox.
    let mut caret_x = 0.0f32;
    let mut outlines = Vec::new();
    let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
    let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
    for c in text.chars() {
        let mut glyph = sf.scaled_glyph(c);
        glyph.position = ab_glyph::point(caret_x, 0.0);
        caret_x += sf.h_advance(glyph.id);
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            min_x = min_x.min(b.min.x);
            min_y = min_y.min(b.min.y);
            max_x = max_x.max(b.max.x);
            max_y = max_y.max(b.max.y);
            outlines.push(outline);
        }
    }
    if outlines.is_empty() {
        return;
    }
    let off_x = (img_w as f32 - (max_x - min_x)) / 2.0 - min_x;
    let off_y = (img_h as f32 - (max_y - min_y)) / 2.0 - min_y;

    for outline in &outlines {
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let x = (b.min.x + gx as f32 + off_x).round() as i32;
            let y = (b.min.y + gy as f32 + off_y).round() as i32;
            blend(buf, img_w, img_h, x, y, color, cov * alpha);
        });
    }
}

/// A small checkered-flag block, centered — used for icon-only / no-data.
fn draw_flag(buf: &mut [u8], w: i32, h: i32, color: [u8; 3], alpha: f32) {
    let cols = 4;
    let rows = 3;
    let cell = 9;
    let bw = cols * cell;
    let bh = rows * cell;
    let ox = (w - bw) / 2;
    let oy = (h - bh) / 2;
    for r in 0..rows {
        for c in 0..cols {
            if (r + c) % 2 == 0 {
                fill_rect(buf, w, h, ox + c * cell, oy + r * cell, cell, cell, color, alpha);
            }
        }
    }
}

/// A thin two-row checker strip across the bottom — a flag nod under the number.
fn draw_checker_strip(buf: &mut [u8], w: i32, h: i32, color: [u8; 3], alpha: f32) {
    let cell = 6;
    let y0 = h - cell;
    let mut c = 0;
    let mut x = 0;
    while x < w {
        if c % 2 == 0 {
            fill_rect(buf, w, h, x, y0, cell, cell, color, alpha * 0.85);
        }
        x += cell;
        c += 1;
    }
}

fn fill_rect(buf: &mut [u8], w: i32, h: i32, x: i32, y: i32, rw: i32, rh: i32, color: [u8; 3], a: f32) {
    for yy in y..(y + rh) {
        for xx in x..(x + rw) {
            blend(buf, w, h, xx, yy, color, a);
        }
    }
}

/// Alpha-composite `color` at `a` over the existing pixel ("over" operator).
fn blend(buf: &mut [u8], w: i32, h: i32, x: i32, y: i32, color: [u8; 3], a: f32) {
    if x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    let a = a.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    let idx = ((y * w + x) * 4) as usize;
    let dst_a = buf[idx] as f32 / 255.0;
    let out_a = a + dst_a * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for i in 0..3 {
        let src = color[i] as f32 / 255.0;
        let dst = buf[idx + 1 + i] as f32 / 255.0;
        let out = (src * a + dst * dst_a * (1.0 - a)) / out_a;
        buf[idx + 1 + i] = (out * 255.0).round() as u8;
    }
    buf[idx] = (out_a * 255.0).round() as u8;
}

// MARK: - Static app icon (for the launcher / notifications / icon theme)

/// A checkered-flag app icon (coral + white), distinct from the dynamic
/// percentage tray icon. Written to a PNG by `--export-icon` at install time.
fn render_app_icon(size: i32) -> IconImage {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let coral = [0xD9, 0x77, 0x57];
    let white = [0xF5, 0xF5, 0xF5];
    let (cols, rows) = (6, 4);
    let cell = ((size as f32) * 0.125) as i32;
    let (bw, bh) = (cols * cell, rows * cell);
    let (ox, oy) = ((size - bw) / 2, (size - bh) / 2);
    for r in 0..rows {
        for c in 0..cols {
            let (color, a) = if (r + c) % 2 == 0 {
                (coral, 1.0)
            } else {
                (white, 0.9)
            };
            fill_rect(&mut buf, size, size, ox + c * cell, oy + r * cell, cell, cell, color, a);
        }
    }
    IconImage {
        width: size,
        height: size,
        argb: buf,
    }
}

/// Encode the app icon as a PNG at `path` (ARGB → RGBA for the encoder).
pub fn write_app_icon(path: &str) -> anyhow::Result<()> {
    let img = render_app_icon(128);
    let mut rgba = Vec::with_capacity(img.argb.len());
    for px in img.argb.chunks_exact(4) {
        rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), img.width as u32, img.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgba)?;
    Ok(())
}
