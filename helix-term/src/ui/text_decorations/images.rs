use std::cell::RefCell;

use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::Position;
use helix_view::graphics::{Color, Style};
use helix_view::inline_image::{ImageBlock, PlacedImage};
use helix_view::media;

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

/// Draws the images of a document (see [`helix_view::inline_image`]) into the rows reserved
/// below their lines by [`helix_view::inline_image::ImageLines`].
///
/// Must be the last decoration that draws virtual lines, matching the annotation being the
/// last that reserves them. The images drawn are pushed to `drawn`, for the caller to
/// transmit once rendering is done (see [`helix_view::media::GraphicsState::ensure_placement`]).
pub struct InlineImages<'a> {
    blocks: Vec<ImageBlock>,
    /// The next block whose line ending has not been passed.
    next: usize,
    /// The block whose line ending was just drawn: its images go in the virtual lines next.
    pending: Option<usize>,
    drawn: &'a RefCell<Vec<PlacedImage>>,
}

impl<'a> InlineImages<'a> {
    pub fn new(blocks: Vec<ImageBlock>, drawn: &'a RefCell<Vec<PlacedImage>>) -> Self {
        Self {
            blocks,
            next: 0,
            pending: None,
            drawn,
        }
    }

    fn next_anchor(&self) -> usize {
        self.blocks
            .get(self.next)
            .map_or(usize::MAX, |block| block.line_end)
    }
}

impl Decoration for InlineImages<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.pending = None;
        self.next = self.blocks.partition_point(|block| block.line_end < pos);
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        if self
            .blocks
            .get(self.next)
            .is_some_and(|block| block.line_end == grapheme.char_idx)
        {
            self.pending = Some(self.next);
            self.next += 1;
        }
        self.next_anchor()
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some(block) = self.pending.take().map(|idx| &self.blocks[idx]) else {
            return Position::new(0, 0);
        };
        let mut row = pos.visual_line as usize + virt_off.row;
        let mut symbol = String::with_capacity(12);
        for image in &block.images {
            let id = image.raster.id;
            // The placeholder's foreground color carries the image id.
            let style =
                Style::default().fg(Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8));
            let mut visible = false;
            for image_row in 0..image.rows {
                for col in 0..image.cols {
                    let Some(chars) = media::placeholder_symbol(image_row, col) else {
                        continue;
                    };
                    symbol.clear();
                    symbol.extend(chars);
                    visible |= renderer.set_cell(row + image_row as usize, col, &symbol, style);
                }
            }
            if visible {
                self.drawn.borrow_mut().push(image.clone());
            }
            row += image.rows as usize;
        }
        Position::new(block.rows() as usize, 0)
    }
}
