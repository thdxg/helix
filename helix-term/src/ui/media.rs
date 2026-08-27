//! Drawing rasterized media (images, PDF pages) into a surface.
//!
//! Both the media view and the picker preview place an image the same way:
//! fit it to the available cells and stamp kitty unicode placeholders over
//! them. See `helix_view::media` for the protocol details.

use helix_view::{
    graphics::{Color, Rect, Style},
    media::{self, GraphicsState, Raster},
};
use tui::buffer::Buffer as Surface;

/// Draw `raster` centred in `area` as kitty unicode-placeholder cells and
/// return the cells it covers. Returns `None` — having drawn nothing — when
/// the terminal has no graphics support, so callers can fall back to text.
///
/// `allow_upscale` is for rasters that can be re-rendered at any size (PDF
/// pages); images are never blown up past their natural size.
pub fn draw_raster(
    surface: &mut Surface,
    graphics: &mut GraphicsState,
    area: Rect,
    raster: &Raster,
    allow_upscale: bool,
) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    // Cell metrics come from the whole terminal, not this area.
    let total = surface.area;
    let cell_px = graphics.cell_px(total.width, total.height);
    let (cols, rows) = media::fit_placement(
        (raster.width, raster.height),
        (area.width, area.height),
        cell_px,
        allow_upscale,
    );
    if !graphics.ensure_placement(raster, cols, rows) {
        return None;
    }

    let placement = Rect::new(
        area.x + (area.width - cols) / 2,
        area.y + (area.height - rows) / 2,
        cols,
        rows,
    );
    // The placeholder's foreground color carries the image id.
    let id_style = Style::default().fg(Color::Rgb(
        (raster.id >> 16) as u8,
        (raster.id >> 8) as u8,
        raster.id as u8,
    ));
    let mut symbol = String::with_capacity(12);
    for row in 0..rows {
        for col in 0..cols {
            let Some(chars) = media::placeholder_symbol(row, col) else {
                continue;
            };
            symbol.clear();
            symbol.extend(chars);
            surface[(placement.x + col, placement.y + row)]
                .set_symbol(&symbol)
                .set_style(id_style);
        }
    }
    Some(placement)
}

/// Display width of a caption, for centring it under a placement. Captions
/// contain non-ASCII characters (`\u{00d7}`, `\u{2014}`, `\u{2026}`), so byte
/// length would push them off centre.
pub fn text_width(text: &str) -> u16 {
    use helix_core::unicode::width::UnicodeWidthStr;
    text.width().try_into().unwrap_or(u16::MAX)
}
