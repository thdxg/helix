//! Code folding.
//!
//! Folding is split in two halves:
//!
//! * **Fold spans** ([`FoldSpan`]) describe what *can* be folded. They are computed on demand
//!   from the document (see [`tree_sitter_spans`] and [`indent_spans`]) and are never stored.
//! * **Closed folds** ([`Folds`]) record which of those spans the user closed. Only the closed
//!   folds need to survive edits, so they are stored as char positions that are mapped through
//!   every [`ChangeSet`].
//!
//! A closed fold keeps its first line (the *header*) visible and hides everything up to and
//! including the last line of the span. When rendering, the hidden text is collapsed into the
//! line ending of the header, see [`crate::doc_formatter`].

use std::cmp::Reverse;

use crate::line_ending::line_end_char_index;
use crate::syntax::{Loader, QueryIterEvent};
use crate::{Assoc, ChangeSet, RopeSlice, Syntax, Tendril};

/// A region of the document that can be folded, as an inclusive range of document lines.
///
/// `first_line` is the header that stays visible when the span is closed, so
/// `first_line < last_line` always holds for a valid span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FoldSpan {
    pub first_line: usize,
    pub last_line: usize,
}

impl FoldSpan {
    pub fn new(first_line: usize, last_line: usize) -> Option<Self> {
        (first_line < last_line).then_some(Self {
            first_line,
            last_line,
        })
    }

    pub fn contains_line(&self, line: usize) -> bool {
        (self.first_line..=self.last_line).contains(&line)
    }
}

/// Sorts spans outermost-first (by start, larger spans before the spans nested in them) and
/// removes duplicates.
fn normalize_spans(spans: &mut Vec<FoldSpan>) {
    spans.sort_unstable_by_key(|span| (span.first_line, Reverse(span.last_line)));
    spans.dedup();
}

fn is_blank(line: RopeSlice) -> bool {
    line.chars().all(char::is_whitespace)
}

/// Computes fold spans from the syntax tree.
///
/// Languages that ship a `folds.scm` decide themselves which nodes fold. For every other
/// language every *named* node that spans more than one line is foldable, except for the root
/// node of a layer (folding it would collapse the whole document into a single line). This
/// needs no per-language configuration and is what neovim's built-in treesitter folding does
/// too. Matches are found per-layer, so nodes belonging to an injected layer are covered as
/// well: a fenced code block in markdown folds like the language of the block does.
pub fn tree_sitter_spans(syntax: &Syntax, text: RopeSlice, loader: &Loader) -> Vec<FoldSpan> {
    let mut spans = Vec::new();
    let mut iter = syntax.query_iter::<_, (), _>(
        text,
        |lang| loader.fold_query(lang).map(|query| &query.query),
        ..,
    );
    while let Some(event) = iter.next() {
        let QueryIterEvent::Match(mat) = event else {
            continue;
        };
        let Some(fold_query) = loader.fold_query(iter.current_language()) else {
            continue;
        };
        if !fold_query.is_fold(mat.capture) || !mat.node.is_named() {
            continue;
        }
        // The generic query matches the root of a layer too, which would fold everything.
        if !fold_query.is_language_specific() && mat.node.parent().is_none() {
            continue;
        }

        let start = text.byte_to_char(mat.node.start_byte() as usize);
        let end = text.byte_to_char(mat.node.end_byte() as usize);
        let first_line = text.char_to_line(start);
        let mut last_line = text.char_to_line(end);
        // Nodes frequently end right after their trailing line break, i.e. at the very start
        // of the next line. That line is not part of the node.
        if end > start && end == text.line_to_char(last_line) {
            last_line -= 1;
        }
        // Keep trailing blank lines visible so that folded siblings stay visually separated.
        while last_line > first_line && is_blank(text.line(last_line)) {
            last_line -= 1;
        }
        spans.extend(FoldSpan::new(first_line, last_line));
    }

    normalize_spans(&mut spans);
    spans
}

/// Computes fold spans from indentation: every line that is followed by more deeply indented
/// lines can be folded together with those lines. Blank lines never end a span.
///
/// This works for anything with an indentation based structure and is used as a fallback for
/// documents without a syntax tree.
pub fn indent_spans(text: RopeSlice, tab_width: usize) -> Vec<FoldSpan> {
    let indent_of = |line: RopeSlice| {
        let mut width = 0;
        for ch in line.chars() {
            match ch {
                '\t' => width += tab_width - width % tab_width,
                ' ' => width += 1,
                _ => break,
            }
        }
        width
    };

    let mut spans = Vec::new();
    // Lines that may still be extended by following, more indented lines: `(indent, line)`.
    let mut open: Vec<(usize, usize)> = Vec::new();
    let mut last_content_line = 0;
    let mut close_until = |open: &mut Vec<(usize, usize)>, indent: usize, last_line: usize| {
        while let Some(&(open_indent, first_line)) = open.last() {
            if indent > open_indent {
                break;
            }
            open.pop();
            spans.extend(FoldSpan::new(first_line, last_line));
        }
    };

    for line_idx in 0..text.len_lines() {
        let line = text.line(line_idx);
        if is_blank(line) {
            continue;
        }
        let indent = indent_of(line);
        close_until(&mut open, indent, last_content_line);
        open.push((indent, line_idx));
        last_content_line = line_idx;
    }
    close_until(&mut open, 0, last_content_line);

    normalize_spans(&mut spans);
    spans
}

/// Returns the spans that contain `line`, outermost first.
pub fn spans_containing_line(
    spans: &[FoldSpan],
    line: usize,
) -> impl DoubleEndedIterator<Item = FoldSpan> + '_ {
    // Spans are sorted by `first_line` so nothing after this point can contain `line`.
    let end = spans.partition_point(|span| span.first_line <= line);
    spans[..end]
        .iter()
        .copied()
        .filter(move |span| span.contains_line(line))
}

/// A closed fold as seen by the renderer.
///
/// Everything in `start..end` is hidden. The header line stays visible, a placeholder
/// is displayed after it and the hidden text is displayed as a part of the header's line ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedRange {
    /// Char index of the first char of the header line.
    header_start: usize,
    /// Char index of the header's line ending: the first hidden char.
    pub start: usize,
    /// Char index of the first char after the fold: the start of the line following the
    /// fold's last line.
    pub end: usize,
    /// The line that stays visible.
    pub header_line: usize,
    /// The last hidden line.
    pub last_line: usize,
    /// Virtual text that is shown at the end of the header line.
    pub placeholder: Tendril,
}

impl FoldedRange {
    pub fn span(&self) -> FoldSpan {
        FoldSpan {
            first_line: self.header_line,
            last_line: self.last_line,
        }
    }

    /// The number of lines that are hidden by this fold.
    pub fn hidden_lines(&self) -> usize {
        self.last_line - self.header_line
    }

    /// Whether `char_idx` is hidden. The header's line ending itself is still displayed so
    /// it is not considered hidden.
    pub fn hides(&self, char_idx: usize) -> bool {
        self.start < char_idx && char_idx < self.end
    }

    /// Recomputes all derived fields from `header_start` and `end`
    /// which are the only fields that are kept in sync with edits.
    fn normalize(&mut self, text: RopeSlice) -> bool {
        let len = text.len_chars();
        let header_line = text.char_to_line(self.header_start.min(len));
        let last_line = text.char_to_line(self.end.min(len).saturating_sub(1));
        if last_line <= header_line {
            return false;
        }

        self.header_line = header_line;
        self.last_line = last_line;
        self.header_start = text.line_to_char(header_line);
        self.start = line_end_char_index(&text, header_line);
        self.end = text.line_to_char(last_line + 1);

        let hidden = self.hidden_lines();
        self.placeholder.clear();
        self.placeholder.push_str(" … ");
        self.placeholder.push_str(&hidden.to_string());
        self.placeholder
            .push_str(if hidden == 1 { " line" } else { " lines" });
        true
    }
}

/// The set of closed folds of a document.
///
/// Folds may be nested. Opening an outer fold leaves inner closed folds closed, like in vim.
#[derive(Debug, Clone, Default)]
pub struct Folds {
    /// All closed folds ordered by `header_start`, outermost first.
    closed: Vec<FoldedRange>,
    /// The subset of `closed` that is not nested in another closed fold.
    /// These are the ranges that are actually hidden and it is what gets rendered.
    folded: Vec<FoldedRange>,
    /// Bumped whenever `folded` is recomputed, so that renderers can cheaply tell whether
    /// the hidden text changed between two frames.
    revision: u64,
}

impl Folds {
    pub fn is_empty(&self) -> bool {
        self.closed.is_empty()
    }

    /// A counter that changes whenever the set of hidden ranges may have changed.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The outermost closed folds. These are sorted and never overlap.
    pub fn folded(&self) -> &[FoldedRange] {
        &self.folded
    }

    /// The closed fold whose (visible) header is `line`, if any.
    pub fn folded_at_header(&self, line: usize) -> Option<&FoldedRange> {
        self.folded
            .binary_search_by_key(&line, |fold| fold.header_line)
            .ok()
            .map(|idx| &self.folded[idx])
    }

    pub fn is_closed(&self, span: FoldSpan) -> bool {
        self.closed.iter().any(|fold| fold.span() == span)
    }

    /// The outermost closed fold that hides `char_idx`.
    pub fn hiding(&self, char_idx: usize) -> Option<&FoldedRange> {
        let idx = self.folded.partition_point(|fold| fold.end <= char_idx);
        self.folded.get(idx).filter(|fold| fold.hides(char_idx))
    }

    /// Closes `span`. Returns whether the span was open before.
    pub fn close(&mut self, text: RopeSlice, span: FoldSpan) -> bool {
        self.close_all(text, [span])
    }

    /// Closes all of `spans`. Returns whether any span was open before.
    pub fn close_all(
        &mut self,
        text: RopeSlice,
        spans: impl IntoIterator<Item = FoldSpan>,
    ) -> bool {
        let len = self.closed.len();
        for span in spans {
            if span.last_line >= text.len_lines() {
                continue;
            }
            let mut fold = FoldedRange {
                header_start: text.line_to_char(span.first_line),
                start: 0,
                end: text.line_to_char(span.last_line + 1),
                header_line: 0,
                last_line: 0,
                placeholder: Tendril::new(),
            };
            if fold.normalize(text) {
                self.closed.push(fold);
            }
        }
        // `rebuild` removes the duplicates of spans that were already closed
        self.rebuild();
        self.closed.len() != len
    }

    /// Opens `span`. Returns whether the span was closed before.
    pub fn open(&mut self, span: FoldSpan) -> bool {
        let len = self.closed.len();
        self.closed.retain(|fold| fold.span() != span);
        let changed = self.closed.len() != len;
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Opens all closed folds that are inside of `span`, including `span` itself.
    /// Returns whether any fold was opened.
    pub fn open_within(&mut self, span: FoldSpan) -> bool {
        let len = self.closed.len();
        self.closed
            .retain(|fold| fold.header_line < span.first_line || fold.last_line > span.last_line);
        let changed = self.closed.len() != len;
        if changed {
            self.rebuild();
        }
        changed
    }

    pub fn open_all(&mut self) {
        self.closed.clear();
        self.folded.clear();
        self.revision += 1;
    }

    /// Opens all closed folds that hide `char_idx` so that it becomes visible.
    /// Returns whether any fold was opened.
    pub fn reveal(&mut self, char_idx: usize) -> bool {
        let len = self.closed.len();
        self.closed.retain(|fold| !fold.hides(char_idx));
        let changed = self.closed.len() != len;
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Returns `char_idx` if it is visible. Otherwise returns the closest position on the
    /// header of the fold that hides it, keeping the column if possible.
    pub fn visible_pos(&self, text: RopeSlice, char_idx: usize) -> usize {
        let Some(fold) = self.hiding(char_idx) else {
            return char_idx;
        };
        let col = char_idx - text.line_to_char(text.char_to_line(char_idx));
        let header_len = fold.start - fold.header_start;
        fold.header_start + col.min(header_len.saturating_sub(1))
    }

    /// Keeps the folds in sync with `changes`, which must already be applied to `text`.
    pub fn map(&mut self, text: RopeSlice, changes: &ChangeSet) {
        if self.closed.is_empty() {
            return;
        }
        // Inserting at the very start of the header line (for example to open a line above)
        // must not push the header line down, and text inserted right after the fold
        // must not become part of it.
        changes.update_positions(
            self.closed
                .iter_mut()
                .map(|fold| (&mut fold.header_start, Assoc::After)),
        );
        changes.update_positions(
            self.closed
                .iter_mut()
                .map(|fold| (&mut fold.end, Assoc::Before)),
        );
        self.refresh(text);
    }

    /// Recomputes the folds after the text changed. Folds that don't span multiple lines
    /// any more are dropped.
    pub fn refresh(&mut self, text: RopeSlice) {
        self.closed.retain_mut(|fold| fold.normalize(text));
        self.rebuild();
    }

    fn rebuild(&mut self) {
        self.revision += 1;
        self.closed
            .sort_unstable_by_key(|fold| (fold.header_start, Reverse(fold.end)));
        self.closed.dedup_by(|a, b| a.span() == b.span());

        self.folded.clear();
        let mut folded_until = 0;
        for fold in &self.closed {
            if fold.start >= folded_until {
                folded_until = fold.end;
                self.folded.push(fold.clone());
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{Rope, Transaction};

    fn span(first_line: usize, last_line: usize) -> FoldSpan {
        FoldSpan {
            first_line,
            last_line,
        }
    }

    #[test]
    fn indent_spans_nested() {
        let text = Rope::from(
            "\
a
  b
    c

    d
  e
f
  g
",
        );
        assert_eq!(
            indent_spans(text.slice(..), 4),
            [span(0, 5), span(1, 4), span(6, 7)]
        );
    }

    #[test]
    fn indent_spans_ignore_trailing_blank_lines() {
        let text = Rope::from("a\n  b\n\n\nc\n");
        assert_eq!(indent_spans(text.slice(..), 4), [span(0, 1)]);
    }

    #[test]
    fn close_normalizes_and_hides() {
        let text = Rope::from("fn a() {\n    1\n    2\n}\nnext\n");
        let mut folds = Folds::default();
        assert!(folds.close(text.slice(..), span(0, 2)));
        assert!(!folds.close(text.slice(..), span(0, 2)));

        let fold = &folds.folded()[0];
        assert_eq!(fold.header_line, 0);
        assert_eq!(fold.last_line, 2);
        // the header's line ending is the first hidden char
        assert_eq!(fold.start, 8);
        assert_eq!(fold.end, text.line_to_char(3));
        assert_eq!(&*fold.placeholder, " … 2 lines");
        assert!(!fold.hides(fold.start));
        assert!(fold.hides(fold.start + 1));
        assert!(!fold.hides(fold.end));
    }

    #[test]
    fn nested_folds_only_outermost_is_rendered() {
        let text = Rope::from("a\n b\n  c\n  d\n e\nf\n");
        let mut folds = Folds::default();
        folds.close(text.slice(..), span(1, 3));
        folds.close(text.slice(..), span(0, 4));
        assert_eq!(folds.folded().len(), 1);
        assert_eq!(folds.folded()[0].header_line, 0);

        // opening the outer fold reveals the inner one, which stays closed
        assert!(folds.open(span(0, 4)));
        assert_eq!(folds.folded().len(), 1);
        assert_eq!(folds.folded()[0].header_line, 1);
    }

    #[test]
    fn close_all_and_open_within() {
        let text = Rope::from("a\n b\n  c\n  d\n e\nf\n g\n");
        let mut folds = Folds::default();
        let spans = [span(0, 4), span(1, 3), span(5, 6)];
        assert!(folds.close_all(text.slice(..), spans));
        // closing again changes nothing
        assert!(!folds.close_all(text.slice(..), spans));
        assert_eq!(folds.folded().len(), 2);

        assert!(folds.open_within(span(0, 4)));
        assert_eq!(folds.folded().len(), 1);
        assert_eq!(folds.folded()[0].span(), span(5, 6));
    }

    #[test]
    fn reveal_opens_every_fold_hiding_a_position() {
        let text = Rope::from("a\n b\n  c\n  d\n e\nf\n");
        let mut folds = Folds::default();
        folds.close(text.slice(..), span(1, 3));
        folds.close(text.slice(..), span(0, 4));
        let in_c = text.line_to_char(2) + 2;
        assert!(folds.hiding(in_c).is_some());
        assert!(folds.reveal(in_c));
        assert!(folds.is_empty());
        assert!(!folds.reveal(in_c));
    }

    #[test]
    fn folds_follow_edits() {
        let mut text = Rope::from("a\nb\nc\nd\ne\n");
        let mut folds = Folds::default();
        folds.close(text.slice(..), span(1, 3));

        // open a line above the header: the fold moves down with its header
        let t = Transaction::change(&text, [(2, 2, Some("x\n".into()))].into_iter());
        assert!(t.changes().apply(&mut text));
        folds.map(text.slice(..), t.changes());
        assert_eq!(folds.folded()[0].span(), span(2, 4));

        // text inserted at the start of the header line stays on the header line
        let t = Transaction::change(&text, [(4, 4, Some("y\n".into()))].into_iter());
        assert!(t.changes().apply(&mut text));
        folds.map(text.slice(..), t.changes());
        assert_eq!(folds.folded()[0].span(), span(3, 5));

        // text inserted after the fold is not hidden
        let after = folds.folded()[0].end;
        let t = Transaction::change(&text, [(after, after, Some("z\n".into()))].into_iter());
        assert!(t.changes().apply(&mut text));
        folds.map(text.slice(..), t.changes());
        assert_eq!(folds.folded()[0].span(), span(3, 5));

        // deleting the whole body removes the fold
        let fold = &folds.folded()[0];
        let t = Transaction::change(&text, [(fold.start, fold.end, None)].into_iter());
        assert!(t.changes().apply(&mut text));
        folds.map(text.slice(..), t.changes());
        assert!(folds.is_empty());
    }

    #[test]
    fn visible_pos_moves_to_header() {
        let text = Rope::from("header\nbody body\nmore\nnext\n");
        let mut folds = Folds::default();
        folds.close(text.slice(..), span(0, 2));
        let in_body = text.line_to_char(1) + 4;
        assert_eq!(folds.visible_pos(text.slice(..), in_body), 4);
        let far_in_body = text.line_to_char(1) + 8;
        // clamped to the last char of the header
        assert_eq!(folds.visible_pos(text.slice(..), far_in_body), 5);
        assert_eq!(folds.visible_pos(text.slice(..), 3), 3);
    }
}

#[cfg(test)]
mod language_test {
    //! These tests exercise `tree_sitter_spans` against real, compiled grammars. They only run
    //! when the grammar `.so`/`.dylib` for the language is present (e.g. after `hx --grammar
    //! build`), which is not guaranteed in every environment, so they skip (rather than fail)
    //! when the grammar isn't available.
    use super::*;
    use crate::config::default_lang_loader;
    use crate::Rope;

    fn spans_for(lang: &str, text: &str) -> Option<Vec<FoldSpan>> {
        let loader = default_lang_loader();
        let lang_id = loader.language_for_name(lang)?;
        let text = Rope::from(text);
        let syntax = Syntax::new(text.slice(..), lang_id, &loader).ok()?;
        Some(tree_sitter_spans(&syntax, text.slice(..), &loader))
    }

    #[test]
    fn rust_function() {
        let Some(spans) = spans_for(
            "rust",
            "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n",
        ) else {
            return;
        };
        // the whole function body, from the opening brace's line to the closing brace's line
        assert!(spans.contains(&FoldSpan::new(0, 3).unwrap()), "{spans:?}");
        // a single-line node (the `let` statement) is not foldable
        assert!(!spans.iter().any(|s| s.first_line == 1), "{spans:?}");
    }

    #[test]
    fn markdown_heading_and_list() {
        let src = "# Title\n\nintro\n\n- one\n  - nested\n- two\n\nafter\n";
        let Some(spans) = spans_for("markdown", src) else {
            return;
        };
        // the whole document is one section headed by the h1
        assert!(spans.contains(&FoldSpan::new(0, 8).unwrap()), "{spans:?}");
        // the outer list item containing the nested one: "fold under this bullet point"
        assert!(spans.contains(&FoldSpan::new(4, 5).unwrap()), "{spans:?}");
    }

    #[test]
    fn markdown_code_fence_folds_using_its_own_language() {
        // no folds.scm is hand-written for either language: the fenced block is walked with
        // rust's own grammar via the injection layer, purely because it's tree-sitter-backed
        let src = "# Title\n\n```rust\nfn f() {\n    1\n}\n```\n";
        let Some(spans) = spans_for("markdown", src) else {
            return;
        };
        // `fn f() { .. }` inside the fence, at the fence's own line offsets
        assert!(spans.contains(&FoldSpan::new(3, 5).unwrap()), "{spans:?}");
    }

    #[test]
    fn folding_works_without_a_hand_written_query() {
        // go has never had a folds.scm written for it in this codebase; folding still works
        // because it falls out of any compiled grammar generically
        let Some(spans) = spans_for(
            "go",
            "func main() {\n\tif true {\n\t\tprintln(\"hi\")\n\t}\n}\n",
        ) else {
            return;
        };
        assert!(spans.contains(&FoldSpan::new(0, 4).unwrap()), "{spans:?}"); // the function
        assert!(spans.contains(&FoldSpan::new(1, 3).unwrap()), "{spans:?}"); // the if statement
    }
}
