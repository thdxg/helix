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

/// A drawn placement: the cells it covers, and how far it can be panned.
pub struct Placement {
    /// The cells the image was drawn into.
    pub area: Rect,
    /// Rows of the placement that are hidden below `area`, i.e. the largest
    /// `pan` that still shows image. Zero when the whole image is visible.
    pub max_pan: u16,
}

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
    draw_raster_panned(surface, graphics, area, raster, allow_upscale, 0)
        .map(|placement| placement.area)
}

/// Draw `raster` in `area`, panned down by `pan` placement rows.
///
/// With `pan == 0` this is [`draw_raster`]: the whole raster is fitted into
/// `area` and centred. With a non-zero `pan` the placement is instead sized to
/// the width of `area`, so an image taller than it is wide overflows
/// vertically and there is something to pan through; the window of `pan
/// ..pan + area.height` placement rows is drawn. Kitty's unicode placeholders
/// name the image row and column each cell shows, so leaving rows out crops
/// the image without re-rasterizing it — the pan is a placement offset, not a
/// new render.
///
/// An image that fits `area` at full width is unaffected by `pan`: fitting to
/// the width and fitting to the whole area agree whenever the result is no
/// taller than `area`, and the returned `max_pan` is then zero so callers can
/// clamp the offset back to the top.
pub fn draw_raster_panned(
    surface: &mut Surface,
    graphics: &mut GraphicsState,
    area: Rect,
    raster: &Raster,
    allow_upscale: bool,
    pan: u16,
) -> Option<Placement> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    // Cell metrics come from the whole terminal, not this area.
    let total = surface.area;
    let cell_px = graphics.cell_px(total.width, total.height);
    // Panning fits the placement to the width alone, letting it grow past the
    // bottom of `area`. `fit_placement` caps the height at the addressable
    // number of placeholder rows.
    let avail = if pan == 0 {
        (area.width, area.height)
    } else {
        (area.width, u16::MAX)
    };
    let (cols, rows) =
        media::fit_placement((raster.width, raster.height), avail, cell_px, allow_upscale);
    if !graphics.ensure_placement(raster, cols, rows) {
        return None;
    }

    let max_pan = rows.saturating_sub(area.height);
    let pan = pan.min(max_pan);
    let visible = rows.min(area.height);

    let placement = Rect::new(
        area.x + (area.width - cols) / 2,
        area.y + (area.height - visible) / 2,
        cols,
        visible,
    );
    // The placeholder's foreground color carries the image id.
    let id_style = Style::default().fg(Color::Rgb(
        (raster.id >> 16) as u8,
        (raster.id >> 8) as u8,
        raster.id as u8,
    ));
    let mut symbol = String::with_capacity(12);
    for row in 0..visible {
        for col in 0..cols {
            let Some(chars) = media::placeholder_symbol(pan + row, col) else {
                continue;
            };
            symbol.clear();
            symbol.extend(chars);
            surface[(placement.x + col, placement.y + row)]
                .set_symbol(&symbol)
                .set_style(id_style);
        }
    }
    Some(Placement {
        area: placement,
        max_pan,
    })
}

/// Display width of a caption, for centring it under a placement. Captions
/// contain non-ASCII characters (`\u{00d7}`, `\u{2014}`, `\u{2026}`), so byte
/// length would push them off centre.
pub fn text_width(text: &str) -> u16 {
    use helix_core::unicode::width::UnicodeWidthStr;
    text.width().try_into().unwrap_or(u16::MAX)
}
