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

use std::ops::Range;

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
pub fn conceal_ranges(
    syntax: &Syntax,
    text: RopeSlice,
    loader: &Loader,
    lines: Range<usize>,
    revealed: impl Fn(usize) -> bool,
) -> Vec<Conceal> {
    let len_lines = text.len_lines();
    let start_line = lines.start.min(len_lines);
    let end_line = lines.end.min(len_lines);
    if start_line >= end_line {
        return Vec::new();
    }
    let start_byte = text.line_to_byte(start_line) as u32;
    let end_byte = if end_line >= len_lines {
        text.len_bytes()
    } else {
        text.line_to_byte(end_line)
    } as u32;

    let mut conceals = Vec::new();
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

    // Injected layers are visited after their parent, so the matches are not in document
    // order. Of overlapping ranges only the first is kept.
    conceals.sort_unstable_by_key(|conceal| (conceal.start, conceal.end));
    let mut last_end = 0;
    conceals.retain(|conceal| {
        let keep = conceal.start >= last_end;
        if keep {
            last_end = conceal.end;
        }
        keep
    });
    conceals
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
}
