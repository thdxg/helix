//! Images drawn inside a document, below the line that references them.
//!
//! While a document is concealed (`editor.markdown-preview`), an image its `conceal.scm` query marks
//! (`@conceal.image`, markdown's `![alt](path)`) is drawn in rows reserved below its line,
//! the way a rendered preview shows it. The rows are virtual lines (a [`LineAnnotation`]), so
//! scrolling and cursor movement step over them like over inline diagnostics, and the image
//! is drawn into them as kitty unicode placeholders (see [`crate::media`]), which crop like
//! ordinary cells when the image is partly scrolled out of view.
//!
//! Only local image files are drawn, resolved against the document's directory; anything
//! else (URLs, PDFs, missing files) keeps showing as the concealed text alone.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use helix_core::conceal::ConcealImage;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::LineAnnotation;
use helix_core::{Position, RopeSlice};

use crate::media::{self, MediaKind, Raster};

/// An image file referenced by a document line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// The document line the image is drawn below.
    pub line: usize,
    /// Where the next document line starts (a char index): the formatter asks for virtual
    /// lines at that position once the line, line ending included, is laid out.
    pub next_line_start: usize,
    /// The char index of the line's line ending, the grapheme the image is drawn after.
    pub line_end: usize,
    pub path: PathBuf,
    pub mtime: SystemTime,
}

/// Turns the images a conceal query found into the image files to draw. `doc_path` is the
/// document's path, which relative paths are resolved against.
pub fn resolve(text: RopeSlice, doc_path: Option<&Path>, images: &[ConcealImage]) -> Vec<ImageRef> {
    let dir = doc_path.and_then(Path::parent);
    images
        .iter()
        .filter_map(|image| {
            let path = local_path(&image.path, dir)?;
            if media::detect_kind(&path) != Some(MediaKind::Image) {
                return None;
            }
            let mtime = path.metadata().ok()?.modified().ok()?;
            // The image is drawn below the line, so a line without a line ending (the last
            // one) has nowhere to draw it.
            let next_line_start = text.try_line_to_char(image.line + 1).ok()?;
            let line_end = line_end_char_index(&text, image.line);
            if next_line_start <= line_end {
                return None;
            }
            Some(ImageRef {
                line: image.line,
                next_line_start,
                line_end,
                path,
                mtime,
            })
        })
        .collect()
}

/// The local file a markdown image destination names, if it names one.
fn local_path(destination: &str, dir: Option<&Path>) -> Option<PathBuf> {
    let destination = destination.trim();
    let destination = destination
        .strip_prefix('<')
        .and_then(|d| d.strip_suffix('>'))
        .unwrap_or(destination);
    let destination = destination.strip_prefix("file://").unwrap_or(destination);
    if destination.is_empty()
        || destination.contains("://")
        || destination.starts_with("data:")
        || destination.starts_with('#')
    {
        return None;
    }
    let decoded = percent_decode(destination);
    let path = helix_stdx::path::expand_tilde(Path::new(&*decoded));
    Some(match dir {
        Some(dir) if path.is_relative() => dir.join(path),
        _ => path.into_owned(),
    })
}

/// Decodes `%XX` escapes, such as the `%20` of a path with spaces.
fn percent_decode(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('%') {
        return s.into();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned().into()
}

/// One image as drawn: its raster, under an id of its own for this size, and the cells it
/// takes.
#[derive(Debug, Clone)]
pub struct PlacedImage {
    pub raster: Raster,
    pub cols: u16,
    pub rows: u16,
}

/// The images drawn below one document line, stacked in the order they appear on it.
#[derive(Debug, Clone)]
pub struct ImageBlock {
    pub line: usize,
    pub next_line_start: usize,
    pub line_end: usize,
    pub images: Vec<PlacedImage>,
}

impl ImageBlock {
    /// The rows the images take together.
    pub fn rows(&self) -> u16 {
        self.images.iter().map(|image| image.rows).sum()
    }
}

/// Sizes the images of `refs` for a view `width` cells wide and `height` rows high.
///
/// An image fills at most the width and half the height of the view and is never drawn
/// larger than its natural size. Images that cannot be drawn yet (still being converted, or
/// the terminal has no graphics) are left out, so they take no rows. Images that take no
/// rows are left out as well.
pub fn layout(refs: &[ImageRef], width: u16, height: u16) -> Vec<ImageBlock> {
    let Some(metrics) = media::graphics_metrics() else {
        return Vec::new();
    };
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let max_rows = (height / 2).max(1);

    let mut blocks: Vec<ImageBlock> = Vec::new();
    for image in refs {
        let Some(mut raster) = media::inline_raster(&image.path, image.mtime) else {
            continue;
        };
        let (cols, rows) = media::fit_placement(
            (raster.width, raster.height),
            (width, max_rows),
            metrics.cell_px,
            false,
        );
        // Kitty keeps one placement size per image, so each size an image is drawn at
        // (views of different widths, the picker preview) is an image of its own.
        raster.id = sized_id(raster.id, cols, rows);
        let placed = PlacedImage { raster, cols, rows };
        match blocks.last_mut() {
            Some(block) if block.line == image.line => block.images.push(placed),
            _ => blocks.push(ImageBlock {
                line: image.line,
                next_line_start: image.next_line_start,
                line_end: image.line_end,
                images: vec![placed],
            }),
        }
    }
    blocks
}

fn sized_id(id: u32, cols: u16, rows: u16) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (id, cols, rows).hash(&mut hasher);
    ((hasher.finish() as u32) & 0xFF_FFFF).max(1)
}

/// Reserves the rows of [`ImageBlock`]s below their lines.
pub struct ImageLines {
    blocks: Vec<ImageBlock>,
}

impl ImageLines {
    pub fn new(blocks: Vec<ImageBlock>) -> Self {
        Self { blocks }
    }
}

impl LineAnnotation for ImageLines {
    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        // Called at the end of every visual line; only the end of the image's own line (not
        // a soft wrap within it, nor the end of a fold that ends where it starts) gets rows.
        let rows = self
            .blocks
            .binary_search_by_key(&doc_line, |block| block.line)
            .ok()
            .map(|idx| &self.blocks[idx])
            .filter(|block| block.next_line_start == line_end_char_idx)
            .map_or(0, |block| block.rows());
        Position::new(rows as usize, 0)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn destinations_resolve_to_local_files() {
        let dir = Path::new("/notes");
        assert_eq!(
            local_path("img/a.png", Some(dir)),
            Some(PathBuf::from("/notes/img/a.png"))
        );
        assert_eq!(
            local_path("<my pic.png>", Some(dir)),
            Some(PathBuf::from("/notes/my pic.png"))
        );
        assert_eq!(
            local_path("my%20pic.png", Some(dir)),
            Some(PathBuf::from("/notes/my pic.png"))
        );
        assert_eq!(
            local_path("/abs/b.png", Some(dir)),
            Some(PathBuf::from("/abs/b.png"))
        );
        assert_eq!(local_path("https://x.y/c.png", Some(dir)), None);
        assert_eq!(local_path("data:image/png;base64,AAA", Some(dir)), None);
        assert_eq!(local_path("", Some(dir)), None);
    }

    #[test]
    fn percent_decoding_leaves_bad_escapes_alone() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%2"), "%2");
    }
}
