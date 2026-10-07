use crate::doc_formatter::{DocumentFormatter, TextFormat};
use crate::fold::{FoldSpan, Folds};
use crate::text_annotations::{InlineAnnotation, Overlay, TextAnnotations};
use crate::Rope;

impl TextFormat {
    fn new_test(softwrap: bool) -> Self {
        TextFormat {
            soft_wrap: softwrap,
            tab_width: 2,
            max_wrap: 3,
            max_indent_retain: 4,
            wrap_indicator: Box::from(""),
            wrap_indicator_highlight: None,
            // use a prime number to allow lining up too often with repeat
            viewport_width: 17,
            soft_wrap_at_text_width: false,
        }
    }
}

impl<'t> DocumentFormatter<'t> {
    fn collect_to_str(&mut self) -> String {
        use std::fmt::Write;
        let mut res = String::new();
        let viewport_width = self.text_fmt.viewport_width;
        let soft_wrap_at_text_width = self.text_fmt.soft_wrap_at_text_width;
        let mut line = 0;

        for grapheme in self {
            if grapheme.visual_pos.row != line {
                line += 1;
                assert_eq!(grapheme.visual_pos.row, line);
                write!(res, "\n{}", ".".repeat(grapheme.visual_pos.col)).unwrap();
            }
            if !soft_wrap_at_text_width {
                assert!(
                    grapheme.visual_pos.col <= viewport_width as usize,
                    "softwrapped failed {}<={viewport_width}",
                    grapheme.visual_pos.col
                );
            }
            write!(res, "{}", grapheme.raw).unwrap();
        }

        res
    }
}

fn softwrap_text(text: &str) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(true),
        &TextAnnotations::default(),
        0,
    )
    .collect_to_str()
}

#[test]
fn basic_softwrap() {
    assert_eq!(
        softwrap_text(&"foo ".repeat(10)),
        "foo foo foo foo \nfoo foo foo foo \nfoo foo  "
    );
    assert_eq!(
        softwrap_text(&"fooo ".repeat(10)),
        "fooo fooo fooo \nfooo fooo fooo \nfooo fooo fooo \nfooo  "
    );

    // check that we don't wrap unnecessarily
    assert_eq!(softwrap_text("\t\txxxx1xxxx2xx\n"), "    xxxx1xxxx2xx \n ");
}

#[test]
fn softwrap_indentation() {
    assert_eq!(
        softwrap_text("\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "    foo1 foo2 \n....foo3 foo4 \n....foo5 foo6 \n "
    );
    assert_eq!(
        softwrap_text("\t\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "      foo1 foo2 \nfoo3 foo4 foo5 \nfoo6 \n "
    );
}

#[test]
fn long_word_softwrap() {
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx2xxxx3xxxx4xxxxx5xxxx6xxxx7xxx8xxxx9xxx\n"),
        "    xxxx1xxxx2xxx\n....x3xxxx4xxxxx5\n....xxxx6xxxx7xxx\n....8xxxx9xxx \n "
    );
    assert_eq!(
        softwrap_text("xxxxxxxx1xxxx2xxx\n"),
        "xxxxxxxx1xxxx2xxx\n \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx 2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxxxxx9xxx\n"),
        "    xxxx1xxxx \n....2xxxx3xxxx4xx\n....xx5xxxx6xxxx7\n....xxxx8xxxxxxx9\n....xxx \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxx 2xxxx3xxxx4xxxxx5xxxx6xxxx7xxxxx8xxxx9xx\n"),
        "    xxxx1xxx 2xxx\n....x3xxxx4xxxxx5\n....xxxx6xxxx7xxx\n....xx8xxxx9xx \n "
    );
}

#[test]
fn softwrap_multichar_grapheme() {
    assert_eq!(
        softwrap_text("xxxx xxxx xxx a\u{0301}bc\n"),
        "xxxx xxxx xxx \nábc \n "
    )
}

fn softwrap_text_at_text_width(text: &str) -> String {
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.soft_wrap_at_text_width = true;
    let annotations = TextAnnotations::default();
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.into(), &text_fmt, &annotations, 0);
    formatter.collect_to_str()
}
#[test]
fn long_word_softwrap_text_width() {
    assert_eq!(
        softwrap_text_at_text_width("xxxxxxxx1xxxx2xxx\nxxxxxxxx1xxxx2xxx"),
        "xxxxxxxx1xxxx2xxx \nxxxxxxxx1xxxx2xxx "
    );
}

fn overlay_text(text: &str, char_pos: usize, softwrap: bool, overlays: &[Overlay]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_overlay(overlays, None),
        char_pos,
    )
    .collect_to_str()
}

#[test]
fn overlay() {
    assert_eq!(
        overlay_text(
            "foobar",
            0,
            false,
            &[Overlay::new(0, "X"), Overlay::new(2, "\t")],
        ),
        "Xo  bar "
    );
    assert_eq!(
        overlay_text(
            &"foo ".repeat(10),
            0,
            true,
            &[
                Overlay::new(2, "\t"),
                Overlay::new(5, "\t"),
                Overlay::new(16, "X"),
            ]
        ),
        "fo   f  o foo \nfoo Xoo foo foo \nfoo foo foo  "
    );
}

fn annotate_text(text: &str, softwrap: bool, annotations: &[InlineAnnotation]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_inline_annotations(annotations, None),
        0,
    )
    .collect_to_str()
}

#[test]
fn annotation() {
    assert_eq!(
        annotate_text("bar", false, &[InlineAnnotation::new(0, "foo")]),
        "foobar "
    );
    assert_eq!(
        annotate_text(
            &"foo ".repeat(10),
            true,
            &[InlineAnnotation::new(0, "foo ")]
        ),
        "foo foo foo foo \nfoo foo foo foo \nfoo foo foo  "
    );
}

#[test]
fn annotation_and_overlay() {
    let annotations = [InlineAnnotation {
        char_idx: 0,
        text: "fooo".into(),
    }];
    let overlay = [Overlay {
        char_idx: 0,
        grapheme: "\t".into(),
    }];
    assert_eq!(
        DocumentFormatter::new_at_prev_checkpoint(
            "bbar".into(),
            &TextFormat::new_test(false),
            TextAnnotations::default()
                .add_inline_annotations(annotations.as_slice(), None)
                .add_overlay(overlay.as_slice(), None),
            0,
        )
        .collect_to_str(),
        "fooo  bar "
    );
}

fn fold_text(
    text: &str,
    softwrap: bool,
    folds: &[(usize, usize)],
    annotations: &[InlineAnnotation],
    char_pos: usize,
) -> String {
    let text = Rope::from(text);
    let mut closed = Folds::default();
    for &(first_line, last_line) in folds {
        let span = FoldSpan::new(first_line, last_line).unwrap();
        assert!(closed.close(text.slice(..), span));
    }
    let text_fmt = TextFormat::new_test(softwrap);
    let mut text_annotations = TextAnnotations::default();
    text_annotations
        .add_inline_annotations(annotations, None)
        .add_folds(closed.folded(), None);
    let res = DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &text_fmt,
        &text_annotations,
        char_pos,
    )
    .collect_to_str();
    res
}

#[test]
fn fold() {
    // the hidden lines are replaced by a placeholder at the end of the header
    assert_eq!(
        fold_text("a\nb\nc\nd\n", false, &[(0, 2)], &[], 0),
        "a … 2 lines \nd \n "
    );
    assert_eq!(
        fold_text("a\nb\nc\nd\n", false, &[(1, 2)], &[], 0),
        "a \nb … 1 line \nd \n "
    );
    // adjacent folds
    assert_eq!(
        fold_text("a\nb\nc\nd\ne\n", false, &[(0, 1), (2, 3)], &[], 0),
        "a … 1 line \nc … 1 line \ne \n "
    );
    // a fold that reaches the end of the text
    assert_eq!(
        fold_text("a\nb\nc\n", false, &[(0, 2)], &[], 0),
        "a … 2 lines \n "
    );
}

#[test]
fn fold_tracks_document_lines() {
    let text = Rope::from("a\nb\nc\nd\ne\n");
    let mut closed = Folds::default();
    closed.close(text.slice(..), FoldSpan::new(1, 3).unwrap());
    let text_fmt = TextFormat::new_test(false);
    let mut text_annotations = TextAnnotations::default();
    text_annotations.add_folds(closed.folded(), None);
    let formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &text_fmt, &text_annotations, 0);
    // (line_idx, char_idx, visual row, doc_chars)
    let graphemes: Vec<_> = formatter
        .filter(|g| !g.is_virtual())
        .map(|g| (g.line_idx, g.char_idx, g.visual_pos.row, g.doc_chars()))
        .collect();
    assert_eq!(
        graphemes,
        [
            (0, 0, 0, 1),  // a
            (0, 1, 0, 1),  // \n
            (1, 2, 1, 1),  // b
            (1, 3, 1, 5),  // the line ending of `b`, which also stands for "\nc\nd\n"
            (4, 8, 2, 1),  // e
            (4, 9, 2, 1),  // \n
            (5, 10, 3, 0), // EOF
        ]
    );
}

#[test]
fn fold_skips_annotations_in_hidden_text() {
    // chars:  a0 \n1 b2 \n3 c4 \n5 d6 \n7 with line 1 (`b`) folded
    let annotations = [
        InlineAnnotation::new(1, "<end>"),
        InlineAnnotation::new(3, "<hid>"),
        InlineAnnotation::new(6, "<aft>"),
    ];
    assert_eq!(
        fold_text("a\nb\nc\nd\n", false, &[(0, 1)], &annotations, 0),
        "a<end> … 1 line \nc \n<aft>d \n "
    );
}

#[test]
fn fold_softwrap() {
    // the placeholder is virtual text and wraps like any other text
    assert_eq!(
        fold_text(
            "aaaa bbbb cccc dddd\nhidden\nnext\n",
            true,
            &[(0, 1)],
            &[],
            0
        ),
        "aaaa bbbb cccc \ndddd … 1 line \nnext \n "
    );
}

#[test]
fn fold_block_starting_in_hidden_text() {
    // starting the formatter inside of a fold starts at the fold's header
    for char_pos in 0..=5 {
        assert_eq!(
            fold_text("a\nb\nc\nd\n", false, &[(0, 2)], &[], char_pos),
            "a … 2 lines \nd \n ",
            "char_pos {char_pos}"
        );
    }
    assert_eq!(fold_text("a\nb\nc\nd\n", false, &[(0, 2)], &[], 6), "d \n ");
}
