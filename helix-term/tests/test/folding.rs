use super::*;
use helix_core::Transaction;
use helix_loader::workspace_trust::WorkspaceTrust;
use helix_term::{application::Application, args::Args};
use helix_view::{current_ref, doc};

// Buffers without a syntax tree fold by indentation, which makes them easy to write down.

/// Runs `keys` against `text` and returns the header/last-line pairs of the closed folds
/// afterwards, for tests that need to check fold state directly rather than through the
/// resulting text and selection (which don't reveal it, since folding doesn't change the text).
async fn folded_spans(text: &str, keys: &str) -> anyhow::Result<Vec<(usize, usize)>> {
    let tc: TestCase = (text, keys, text).into();
    let spans = std::cell::RefCell::new(Vec::new());
    test_key_sequence_with_input_text(
        None,
        tc,
        &|app| {
            let (view, doc) = current_ref!(app.editor);
            *spans.borrow_mut() = doc
                .folds(view.id)
                .folded()
                .iter()
                .map(|fold| (fold.header_line, fold.last_line))
                .collect();
        },
        false,
    )
    .await?;
    Ok(spans.into_inner())
}

/// The primary range's `(anchor, head)`, ignoring `old_visual_position` (which vertical motions
/// set and only certain other motions clear, so it isn't meaningful to assert on here).
async fn primary_range(text: &str, keys: &str) -> anyhow::Result<(usize, usize)> {
    let tc: TestCase = (text, keys, text).into();
    let range = std::cell::Cell::new((0, 0));
    test_key_sequence_with_input_text(
        None,
        tc,
        &|app| {
            let doc = doc!(app.editor);
            let primary = doc.selections().values().next().unwrap().primary();
            range.set((primary.anchor, primary.head));
        },
        false,
    )
    .await?;
    Ok(range.get())
}

#[tokio::test(flavor = "multi_thread")]
async fn folds_are_per_view() -> anyhow::Result<()> {
    // splitting a view carries its folds over to the new view (matching vim/neovim, where a
    // new window inherits the folds of the window it was split from), but afterwards the two
    // views' folds are independent, like two vim windows on the same buffer
    let mut app = Application::new(
        Args::default(),
        test_config(),
        test_syntax_loader(None),
        WorkspaceTrust::fully_trusted(),
    )?;
    let (view, doc) = helix_view::current!(app.editor);
    let sel = doc.selection(view.id).clone();
    let text = "one\n  two\n  three\nfour\n";
    let transaction = Transaction::change_by_selection(doc.text(), &sel, |_| {
        (0, doc.text().len_chars(), Some(text.into()))
    })
    .with_selection(Selection::point(0));
    doc.apply(&transaction, view.id);

    let check_two_independent_views = |app: &Application| {
        let doc = app.editor.documents().next().unwrap();
        let views: Vec<_> = app.editor.tree.views().map(|(view, _)| view.id).collect();
        assert_eq!(views.len(), 2, "expected a split to produce two views");

        let closed_in: Vec<_> = views.iter().map(|&id| !doc.folds(id).is_empty()).collect();
        // exactly one of the two views still has the fold closed (the one focus was returned
        // to), and one has it open (the split, where it was opened) -- not both open (which
        // `zo` in the split alone would give if folds were shared) and not both closed (which
        // would mean the split never inherited the fold to begin with)
        assert_eq!(
            closed_in.iter().filter(|&&closed| closed).count(),
            1,
            "expected exactly one view to still have the fold closed, got {closed_in:?}"
        );
    };

    test_key_sequences(
        &mut app,
        vec![
            (
                // close the fold, split (inheriting it), open it only in the split, come back
                Some("zc<C-w>vzo<C-w>w"),
                Some(&check_two_independent_views as &dyn Fn(&Application)),
            ),
            // close the split so the framework's own exit sequence can quit cleanly
            (Some("<C-w>q"), None),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn vertical_movement_skips_closed_fold() -> anyhow::Result<()> {
    let text = indoc! {"\
        #[o|]#ne
          two
          three
        four
        five
    "};

    // without folding `j` visits every line
    test((text, "j;", "one\n#[ |]# two\n  three\nfour\nfive\n")).await?;
    // `j` moves from the fold's header to the line after the fold, `k` back to the header
    test((text, "zcj;", "one\n  two\n  three\n#[f|]#our\nfive\n")).await?;
    test((text, "zcjk;", "#[o|]#ne\n  two\n  three\nfour\nfive\n")).await?;
    // a count moves over the (multi-line) fold like it was a single visible line: 2j from the
    // header goes to "five", the line after the line after the fold, not to "three" (which is
    // hidden) or beyond "five"
    test((text, "zc2j;", "one\n  two\n  three\nfour\n#[f|]#ive\n")).await?;
    // opening the fold again restores the normal behaviour
    test((text, "zczoj;", "one\n#[ |]# two\n  three\nfour\nfive\n")).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_fold_moves_cursor_to_header() -> anyhow::Result<()> {
    test((
        "one\n  #[t|]#wo\n  three\nfour\n",
        "zc",
        "on#[e|]#\n  two\n  three\nfour\n",
    ))
    .await?;

    // the innermost open fold around the cursor is closed
    let nested = indoc! {"\
        a
          b
            #[c|]#
            d
          e
        f
    "};
    test((nested, "zc", "a\n  #[b|]#\n    c\n    d\n  e\nf\n")).await?;
    // closing again closes the fold around that one
    test((nested, "zczc", "#[a|]#\n  b\n    c\n    d\n  e\nf\n")).await?;
    // `zC` closes all of them at once
    test((nested, "zC", "#[a|]#\n  b\n    c\n    d\n  e\nf\n")).await?;
    // `zo` only opens the outermost fold: the inner one is still closed and `j` skips its body
    test((nested, "zCzoj;", "a\n#[ |]# b\n    c\n    d\n  e\nf\n")).await?;
    test((nested, "zCzojj;", "a\n  b\n    c\n    d\n#[ |]# e\nf\n")).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn toggle_and_open_close_all() -> anyhow::Result<()> {
    let text = "#[o|]#ne\n  two\n  three\nfour\n";
    test((text, "zaj;", "one\n  two\n  three\n#[f|]#our\n")).await?;
    test((text, "zazaj;", "one\n#[ |]# two\n  three\nfour\n")).await?;

    let two_folds = "#[o|]#ne\n  two\nthree\n  four\n";
    // the first `j` skips the first fold and lands on the second fold's header; like vanilla
    // `j` on the last line, the second `j` then overshoots to the end of the document
    test((two_folds, "zMj;", "one\n  two\n#[t|]#hree\n  four\n")).await?;
    test((two_folds, "zMjj;", "one\n  two\nthree\n  four\n#[|]#")).await?;
    test((two_folds, "zMzRjj;", "one\n  two\n#[t|]#hree\n  four\n")).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn open_recursively_from_a_visible_line_is_scoped_to_its_section() -> anyhow::Result<()> {
    // "a" has two children, "b" (with its own nested fold) and "e"; "f" is an unrelated,
    // separately-folded sibling section
    let nested = "#[a|]#\n  b\n    c\n    d\n  e\nf\n  g\n    h\n    i\n";

    // closing everything folds both sections
    assert_eq!(folded_spans(nested, "zM").await?, [(0, 4), (5, 8)]);
    // opening "a" only opens the outer fold, leaving "b"'s nested fold closed
    assert_eq!(folded_spans(nested, "zMzo").await?, [(1, 3), (5, 8)]);
    // `zO` from "e" (not a fold header, but inside "a"'s section) opens "b"'s fold too, since
    // it is nested in the same section as the cursor, but leaves the unrelated "f" section alone
    assert_eq!(folded_spans(nested, "zMzojjzO").await?, [(5, 8)]);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_fold_preserves_an_extended_selections_anchor() -> anyhow::Result<()> {
    // "a" is the header of the outer fold(0,4); "b" is the header of the inner fold(1,3),
    // which hides "c" and "d"
    let nested = "#[a|]#\n  b\n    c\n    d\n  e\nf\n";

    // extend a selection from "a" down to "c" (anchor 0, head 7: the block cursor lands on the
    // first char of "c"'s line, one past its own grapheme per block-cursor convention)
    assert_eq!(primary_range(nested, "vjj").await?, (0, 7));
    // closing the innermost fold around the head ("c") hides it; because the selection is wider
    // than a single grapheme, only the head moves onto the fold's header ("b"), the anchor on
    // "a" is untouched, not collapsed into a single cursor
    assert_eq!(primary_range(nested, "vjjzc").await?, (0, 2));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_into_a_fold_opens_it() -> anyhow::Result<()> {
    // search
    test((
        "#[o|]#ne\n  two\n  three\nfour\n",
        "zc/three<ret>kgh",
        "one\n#[ |]# two\n  three\nfour\n",
    ))
    .await?;
    // goto line
    test((
        "#[o|]#ne\n  two\n  three\nfour\n",
        "zc3gg",
        "one\n  two\n#[ |]# three\nfour\n",
    ))
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn folds_follow_edits() -> anyhow::Result<()> {
    // opening a line above the header keeps the fold on its header
    test((
        "#[o|]#ne\n  two\n  three\nfour\n",
        "zcOx<esc>jjgh",
        "x\none\n  two\n  three\n#[f|]#our\n",
    ))
    .await?;

    // typing at the end of the header does not reveal the fold or hide the new text
    test((
        "#[o|]#ne\n  two\n  three\nfour\n",
        "zcAxy<esc>jgh",
        "onexy\n  two\n  three\n#[f|]#our\n",
    ))
    .await?;

    Ok(())
}
