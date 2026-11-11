//! Hiding markup ("conceal").
//!
//! Markup languages such as markdown spell their formatting out in the text: `**bold**`,
//! `# Heading`, `[text](url)`. With `editor.conceal` on, the markers are hidden while the
//! text they mark keeps its highlight, so a document reads like a rendered preview. Lines
//! that hold a cursor (or any part of a selection) are shown as they are in the file, so the
//! source is always visible where it is being edited, and the rest of the document is
//! concealed.
//!
//! What to hide is decided per language by a `conceal.scm` query, see
//! [`crate::syntax::ConcealQuery`]. The hidden ranges become [`Conceal`] annotations that the
//! [`crate::doc_formatter`] collapses into zero width (or a replacement glyph), so that
//! rendering, scrolling and cursor movement all agree on the layout.

use std::borrow::Cow;
use std::ops::Range;

use helix_stdx::rope::RopeSliceExt;
use tree_house::tree_sitter::Node;

use crate::graphemes::grapheme_width;
use crate::line_ending::line_end_char_index;
use crate::syntax::{Loader, QueryIterEvent};
use crate::text_annotations::Conceal;
use crate::{RopeSlice, Syntax};

/// Computes the hidden text of the document `lines` (end exclusive).
///
/// `revealed` decides, per document line, whether the line is shown as it is in the file
/// instead of concealed. Ranges that would span a line break are never hidden. The result is
/// sorted and non-overlapping, as [`crate::text_annotations::TextAnnotations::add_conceals`]
/// requires.
///
/// Tables (`@conceal.table`) have their columns lined up: the cells of a column are padded
/// to the widest one as it is displayed, after its own markup is hidden. See
/// [`table_conceals`].
pub fn conceal_ranges(
    syntax: &Syntax,
    text: RopeSlice,
    loader: &Loader,
    lines: Range<usize>,
    revealed: impl Fn(usize) -> bool,
) -> Vec<Conceal> {
    let len_lines = text.len_lines();
    let lines = lines.start.min(len_lines)..lines.end.min(len_lines);
    if lines.is_empty() {
        return Vec::new();
    }

    let (mut conceals, tables) = query_conceals(syntax, text, loader, lines.clone(), &revealed);
    let mut table_extra = Vec::new();
    for table in tables {
        table_conceals(
            syntax,
            text,
            loader,
            table,
            &lines,
            &revealed,
            &mut conceals,
            &mut table_extra,
        );
    }
    conceals.extend(table_extra);
    sort_and_dedup(&mut conceals);
    conceals
}

/// Runs the conceal query over `lines`: the hidden ranges, sorted and non-overlapping, and
/// the tables (`@conceal.table`) that intersect the lines.
fn query_conceals<'tree>(
    syntax: &'tree Syntax,
    text: RopeSlice<'tree>,
    loader: &'tree Loader,
    lines: Range<usize>,
    revealed: &dyn Fn(usize) -> bool,
) -> (Vec<Conceal>, Vec<Node<'tree>>) {
    let len_lines = text.len_lines();
    let start_byte = text.line_to_byte(lines.start) as u32;
    let end_byte = if lines.end >= len_lines {
        text.len_bytes()
    } else {
        text.line_to_byte(lines.end)
    } as u32;

    let mut conceals = Vec::new();
    let mut tables: Vec<Node<'tree>> = Vec::new();
    let mut iter = syntax.query_iter::<_, (), _>(
        text,
        |lang| loader.conceal_query(lang).map(|query| &query.query),
        start_byte..end_byte,
    );
    while let Some(event) = iter.next() {
        let QueryIterEvent::Match(mat) = event else {
            continue;
        };
        let Some(query) = loader.conceal_query(iter.current_language()) else {
            continue;
        };
        if query.is_table(mat.capture) {
            if !tables.contains(&mat.node) {
                tables.push(mat.node.clone());
            }
            continue;
        }
        if !query.is_conceal(mat.capture) {
            continue;
        }

        let mut start = text.byte_to_char(mat.node.start_byte() as usize);
        let mut end = text.byte_to_char(mat.node.end_byte() as usize);
        if query.trims_whitespace(mat.pattern) {
            while start < end && text.char(start).is_whitespace() {
                start += 1;
            }
            while end > start && text.char(end - 1).is_whitespace() {
                end -= 1;
            }
        }
        if query.hides_trailing_whitespace(mat.pattern) {
            while end < text.len_chars() && matches!(text.char(end), ' ' | '\t') {
                end += 1;
            }
        }
        if start >= end {
            continue;
        }

        let line = text.char_to_line(start);
        // Hiding a line break would merge two lines into one row.
        if end > line_end_char_index(&text, line) || revealed(line) {
            continue;
        }
        conceals.push(Conceal::new(start, end, query.replacement(mat.pattern)));
    }
    drop(iter);

    sort_and_dedup(&mut conceals);
    (conceals, tables)
}

/// Injected layers are visited after their parent, so matches are not in document order.
/// Of overlapping ranges only the first is kept.
fn sort_and_dedup(conceals: &mut Vec<Conceal>) {
    conceals.sort_unstable_by_key(|conceal| (conceal.start, conceal.end));
    let mut last_end = 0;
    conceals.retain(|conceal| {
        let keep = conceal.start >= last_end;
        if keep {
            last_end = conceal.end;
        }
        keep
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// One row of a table: the char positions of its `|` separators and its cells, the text
/// between them. A cell before the first or after the last separator only counts when it is
/// not blank, so `| a | b |` and `a | b` both have two cells.
struct TableRow {
    line: usize,
    delimiter: bool,
    pipes: Vec<usize>,
    cells: Vec<TableCell>,
}

struct TableCell {
    range: Range<usize>,
    /// Index into [`TableRow::pipes`] of the separator before the cell.
    prev_pipe: Option<usize>,
}

impl TableRow {
    fn new(node: &Node, text: RopeSlice) -> Self {
        let char_at = |byte: u32| text.byte_to_char(byte as usize);
        let start = char_at(node.start_byte());
        let end = char_at(node.end_byte());
        let pipes: Vec<usize> = node
            .children()
            .filter(|child| !child.is_named() && child.kind() == "|")
            .map(|child| char_at(child.start_byte()))
            .collect();

        let is_blank = |range: &Range<usize>| {
            text.slice(range.clone())
                .chars()
                .all(|ch| ch.is_whitespace())
        };
        let mut cells = Vec::new();
        let mut cell_start = start;
        for (idx, &pipe) in pipes.iter().enumerate() {
            let range = cell_start..pipe;
            if idx > 0 || !is_blank(&range) {
                cells.push(TableCell {
                    range,
                    prev_pipe: idx.checked_sub(1),
                });
            }
            cell_start = pipe + 1;
        }
        let range = cell_start..end.max(cell_start);
        if !is_blank(&range) {
            cells.push(TableCell {
                range,
                prev_pipe: pipes.len().checked_sub(1),
            });
        }

        Self {
            line: text.char_to_line(start),
            delimiter: node.kind() == "pipe_table_delimiter_row",
            pipes,
            cells,
        }
    }
}

/// The display width of `range` with `conceals` (sorted) applied.
fn concealed_width(text: RopeSlice, range: Range<usize>, conceals: &[Conceal]) -> usize {
    let mut idx = conceals.partition_point(|conceal| conceal.start < range.start);
    let mut pos = range.start;
    let mut width = 0;
    for grapheme in text.slice(range.clone()).graphemes() {
        let len = grapheme.len_chars();
        if let Some(conceal) = conceals.get(idx).filter(|conceal| conceal.start == pos) {
            width += str_width(&conceal.replacement);
            idx += 1;
        } else if !conceals
            .get(idx.wrapping_sub(1))
            .is_some_and(|conceal| conceal.start <= pos && pos < conceal.end)
        {
            width += grapheme_width(&Cow::from(grapheme));
        }
        pos += len;
    }
    width
}

fn str_width(s: &str) -> usize {
    unicode_segmentation::UnicodeSegmentation::graphemes(s, true)
        .map(grapheme_width)
        .sum()
}

/// Lines up the columns of a tree-sitter-markdown `pipe_table`.
///
/// Every cell is padded to the display width of the widest cell of its column, measured
/// with the cell's own markup hidden, honouring the column's `:---:` alignment. The `|`
/// separators become `│` and the delimiter row a `├───┼───┤` line. Rows on revealed lines
/// keep their source text. Only rows on `lines` get conceals; the column widths come from
/// the whole table so they do not change as the table scrolls or the cursor moves.
///
/// Padding is added to the replacement of a conceal on the last char of a cell (after the
/// cell text) or on the `│` before it (before the cell text), so that the replacement's
/// first grapheme, which takes the highlight of the hidden text, is the visible one.
/// `conceals` must be sorted; padding may be added to them in place and new conceals are
/// pushed to `extra`.
#[allow(clippy::too_many_arguments)]
fn table_conceals(
    syntax: &Syntax,
    text: RopeSlice,
    loader: &Loader,
    table: Node,
    lines: &Range<usize>,
    revealed: &dyn Fn(usize) -> bool,
    conceals: &mut [Conceal],
    extra: &mut Vec<Conceal>,
) {
    let rows: Vec<TableRow> = table
        .children()
        .filter(|child| {
            matches!(
                child.kind(),
                "pipe_table_header" | "pipe_table_row" | "pipe_table_delimiter_row"
            )
        })
        .map(|row| TableRow::new(&row, text))
        .collect();
    if !rows
        .iter()
        .any(|row| lines.contains(&row.line) && !revealed(row.line))
    {
        return;
    }

    let aligns: Vec<Align> = table
        .children()
        .find(|child| child.kind() == "pipe_table_delimiter_row")
        .map(|row| {
            row.children()
                .filter(|child| child.kind() == "pipe_table_delimiter_cell")
                .map(|cell| {
                    let has = |kind| cell.children().any(|child| child.kind() == kind);
                    match (has("pipe_table_align_left"), has("pipe_table_align_right")) {
                        (true, true) => Align::Center,
                        (false, true) => Align::Right,
                        _ => Align::Left,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // The widths are measured with every row concealed, revealed or not, so that the
    // columns stay put when the cursor moves through the table.
    let first_line = rows.first().map_or(0, |row| row.line);
    let last_line = rows.last().map_or(0, |row| row.line);
    let (table_conceals, _) =
        query_conceals(syntax, text, loader, first_line..last_line + 1, &|_| false);
    let mut widths: Vec<usize> = Vec::new();
    let mut cell_widths: Vec<Vec<usize>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let row_widths: Vec<usize> = row
            .cells
            .iter()
            .map(|cell| concealed_width(text, cell.range.clone(), &table_conceals))
            .collect();
        if !row.delimiter {
            for (col, &width) in row_widths.iter().enumerate() {
                if col >= widths.len() {
                    widths.push(0);
                }
                widths[col] = widths[col].max(width);
            }
        }
        cell_widths.push(row_widths);
    }

    for (row, row_widths) in rows.iter().zip(&cell_widths) {
        if !lines.contains(&row.line) || revealed(row.line) {
            continue;
        }
        if row.delimiter {
            for (col, cell) in row.cells.iter().enumerate() {
                let width = widths.get(col).copied().unwrap_or(0).max(1);
                extra.push(Conceal::new(
                    cell.range.start,
                    cell.range.end,
                    "─".repeat(width),
                ));
            }
            let first_cell = row
                .cells
                .first()
                .map_or(usize::MAX, |cell| cell.range.start);
            let last_cell = row.cells.last().map_or(0, |cell| cell.range.end);
            for &pipe in &row.pipes {
                let bar = if pipe < first_cell {
                    "├"
                } else if pipe >= last_cell {
                    "┤"
                } else {
                    "┼"
                };
                extra.push(Conceal::new(pipe, pipe + 1, bar));
            }
            continue;
        }

        let mut pipes: Vec<String> = vec!["│".to_string(); row.pipes.len()];
        for (col, (cell, &width)) in row.cells.iter().zip(row_widths).enumerate() {
            let pad = widths
                .get(col)
                .copied()
                .unwrap_or(width)
                .saturating_sub(width);
            if pad == 0 {
                continue;
            }
            let (before, mut after) = match aligns.get(col).copied().unwrap_or(Align::Left) {
                Align::Left => (0, pad),
                Align::Right => (pad, 0),
                Align::Center => (pad / 2, pad - pad / 2),
            };
            if before > 0 {
                if let Some(prev) = cell.prev_pipe {
                    pipes[prev].push_str(&" ".repeat(before));
                } else if !cell.range.is_empty() && text.char(cell.range.start) == ' ' {
                    extra.push(Conceal::new(
                        cell.range.start,
                        cell.range.start + 1,
                        " ".repeat(before + 1),
                    ));
                } else {
                    // Nothing before the text to carry the padding: align it left.
                    after += before;
                }
            }
            // Padding after the last cell of a row without a closing `|` would be invisible.
            if after == 0 || !row.pipes.contains(&cell.range.end) {
                continue;
            }
            if cell.range.is_empty() {
                // `||`: nothing to pad after, but the separator before can carry it.
                if let Some(prev) = cell.prev_pipe {
                    pipes[prev].push_str(&" ".repeat(after));
                }
                continue;
            }
            let last = cell.range.end - 1;
            let covering = conceals.partition_point(|conceal| conceal.start <= last);
            match covering
                .checked_sub(1)
                .map(|idx| &mut conceals[idx])
                .filter(|conceal| last < conceal.end)
            {
                Some(conceal) => {
                    let mut replacement = conceal.replacement.to_string();
                    replacement.push_str(&" ".repeat(after));
                    conceal.replacement = replacement.into();
                }
                None => {
                    let mut replacement = text.char(last).to_string();
                    replacement.push_str(&" ".repeat(after));
                    extra.push(Conceal::new(last, last + 1, replacement));
                }
            }
        }
        for (&pipe, bar) in row.pipes.iter().zip(pipes) {
            extra.push(Conceal::new(pipe, pipe + 1, bar));
        }
    }
}

#[cfg(test)]
mod test {
    use once_cell::sync::Lazy;

    use super::*;
    use crate::Rope;

    static LOADER: Lazy<Loader> = Lazy::new(crate::config::default_lang_loader);

    /// The conceals of a markdown document as `(hidden text, replacement)` pairs, in
    /// document order, with the given lines revealed.
    fn markdown_conceals(src: &str, revealed: &[usize]) -> Vec<(String, String)> {
        let text = Rope::from(src);
        let language = LOADER.language_for_name("markdown").unwrap();
        let syntax = Syntax::new(text.slice(..), language, &LOADER).unwrap();
        let lines = text.len_lines();
        conceal_ranges(&syntax, text.slice(..), &LOADER, 0..lines, |line| {
            revealed.contains(&line)
        })
        .into_iter()
        .map(|conceal| {
            (
                text.slice(conceal.start..conceal.end).to_string(),
                conceal.replacement.to_string(),
            )
        })
        .collect()
    }

    fn hidden(conceals: &[(String, String)]) -> Vec<&str> {
        conceals.iter().map(|(hidden, _)| hidden.as_str()).collect()
    }

    #[test]
    fn inline_markup_is_hidden() {
        let conceals = markdown_conceals(
            "Some *italic*, **bold**, ~~struck~~ and `code` with a [link](https://x.y).\n",
            &[],
        );
        assert_eq!(
            conceals
                .iter()
                .map(|(hidden, replacement)| (hidden.as_str(), replacement.as_str()))
                .collect::<Vec<_>>(),
            // the grammar emits one delimiter node per `*` and `~`
            [
                ("*", ""),
                ("*", ""),
                ("*", ""),
                ("*", ""),
                ("*", ""),
                ("*", ""),
                ("~", ""),
                ("~", ""),
                ("~", ""),
                ("~", ""),
                ("`", ""),
                ("`", ""),
                ("[", ""),
                ("]", " "),
                ("(", ""),
                (")", ""),
            ]
        );
    }

    #[test]
    fn block_markup_is_hidden_or_replaced() {
        let conceals = markdown_conceals(
            "# Title\n\n- item\n- [ ] todo\n- [x] done\n\n> quote\n> more\n\n```rust\nfn main() {}\n```\n",
            &[],
        );
        assert_eq!(
            conceals
                .iter()
                .map(|(hidden, replacement)| (hidden.as_str(), replacement.as_str()))
                .collect::<Vec<_>>(),
            [
                ("# ", ""),
                ("-", "•"),
                ("-", "•"),
                ("-", "•"),
                ("[x]", "[✓]"),
                (">", "│"),
                (">", "│"),
                ("```", ""),
                ("rust", ""),
                ("```", ""),
            ]
        );
    }

    #[test]
    fn revealed_lines_keep_their_markup() {
        let conceals = markdown_conceals("# Title\n\n**bold**\n", &[2]);
        assert_eq!(hidden(&conceals), ["# "]);
    }

    #[test]
    fn multi_line_markup_is_never_hidden() {
        // A link whose text wraps onto the next line: hiding `[`..`]` is fine but the
        // brackets must stay independent of each other, and no range may contain the newline.
        let conceals = markdown_conceals("[two\nlines](https://x.y)\n", &[]);
        for (hidden, _) in &conceals {
            assert!(!hidden.contains('\n'), "{hidden:?} spans a line break");
        }
    }

    /// Renders `src` the way the formatter displays it: every conceal replaced by its
    /// replacement.
    fn rendered(src: &str, revealed: &[usize]) -> String {
        let text = Rope::from(src);
        let language = LOADER.language_for_name("markdown").unwrap();
        let syntax = Syntax::new(text.slice(..), language, &LOADER).unwrap();
        let conceals = conceal_ranges(
            &syntax,
            text.slice(..),
            &LOADER,
            0..text.len_lines(),
            |line| revealed.contains(&line),
        );
        let mut out = String::new();
        let mut pos = 0;
        for conceal in conceals {
            out.extend(text.slice(pos..conceal.start).chars());
            out.push_str(&conceal.replacement);
            pos = conceal.end;
        }
        out.extend(text.slice(pos..).chars());
        out
    }

    #[test]
    fn table_columns_are_lined_up() {
        let src = "| Name | Notes |\n|---|---|\n| **bold** | a [link](u) |\n| x | y |\n";
        assert_eq!(
            rendered(src, &[]),
            "│ Name │ Notes    │\n├──────┼──────────┤\n│ bold │ a link u │\n│ x    │ y        │\n"
        );
    }

    #[test]
    fn table_alignment_is_honoured() {
        let src = "| l | c | r |\n|:--|:-:|--:|\n| long | wider | widest |\n";
        assert_eq!(
            rendered(src, &[]),
            "│ l    │   c   │      r │\n├──────┼───────┼────────┤\n│ long │ wider │ widest │\n"
        );
    }

    #[test]
    fn table_without_outer_pipes() {
        let src = "a | bb\n--|--\nccc | d\n";
        assert_eq!(rendered(src, &[]), "a   │ bb\n────┼───\nccc │ d\n");
    }

    #[test]
    fn revealed_table_row_keeps_its_source() {
        let src = "| a | b |\n|---|---|\n| **long** | c |\n";
        assert_eq!(
            rendered(src, &[2]),
            "│ a    │ b │\n├──────┼───┤\n| **long** | c |\n"
        );
    }
}
