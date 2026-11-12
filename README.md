# Helix (thdxg fork)

A fork of [mattwparas/helix](https://github.com/mattwparas/helix)'s
`steel-event-system` branch (Helix plus the [Steel](https://github.com/mattwparas/steel)
plugin system). Everything below is added on top of it. Options are documented
in [`book/src/editor.md`](book/src/editor.md) and [`book/src/keymap.md`](book/src/keymap.md).

## Features

**Files**
- **Auto-reload on external change.** File watching reloads unmodified buffers
  when the file changes on disk; modified buffers get a conflict warning
  (`[editor.auto-reload]`).
- **Deleted files keep their buffer.** Like Vim, the buffer stays open and is
  marked modified; `:w` recreates the file.
- **Live git gutter.** The diff gutter updates when git state changes.

**Images and PDFs**
- Open them with `:open` and they render in the view through the kitty graphics
  protocol. Page PDFs with `h`/`k`, `j`/`l`, the mouse wheel, goto-line keys,
  `:media-next-page`, `:media-prev-page` and `:media-goto-page N`.
- The file picker previews them too. Requires `pdftoppm` for PDFs and
  `magick`/`convert`/`sips` for non-PNG images (`editor.image-rendering`).

**Pickers**
- **Modal pickers.** Opt in with `editor.picker.modal` to drive pickers with
  bare keys; `default-mode` sets whether they open in insert or normal mode.
- **File explorer.** Create, delete, move, copy, cut, paste (yazi-style numbered
  names, `P` to overwrite) and yank paths. `h`/`l` walk the tree, symlinks are
  marked, and the tree rereads when it changes on disk. It can open on the
  current buffer.
- **Buffer line search** and a **scrollable file preview**.

**Editing and rendering**
- **Code folding** from tree-sitter, with `folds.scm` queries per language.
- **Markdown preview.** Markdown reads like rendered text: markup such as `**`
  and `#` is hidden except on lines with a cursor or selection, tables are
  aligned and boxed, and local images are drawn below their lines
  (`editor.markdown-preview`, `:toggle markdown-preview`).
- **Terminal scroll regions** for scrolling, so terminals that animate scrolling
  show the motion (`editor.smooth-scroll`).
- **Soft-wrap indicators** in the line-number gutter.
- **`native` cursor shape**, which leaves the cursor to the terminal.
- **Alloy 6** as the `als` language, plus Go import path highlighting.

**Performance and plugins**
- **Fast `:q`.** The LSP shutdown wait is capped, and a PDF still rendering no
  longer holds up quit.
- **Steel boots in the background**, and `init.scm` runs after the first frame.
- **Plugin hooks.** Steel plugins can handle file-picker opens
  (`set-file-picker-open-handler!`). Bundled plugins:
  [`macterm-splits.hx`](https://github.com/thdxg/macterm-splits.hx) and
  [`comment.hx`](https://github.com/thdxg/comment.hx).

## Building

```sh
git clone --recurse-submodules https://github.com/thdxg/helix
cd helix
cargo install --path helix-term --locked
```

See the [installation docs](https://docs.helix-editor.com/install.html) for
runtime and grammar setup.

## Credits

Built on [Helix](https://helix-editor.com) and
[mattwparas/helix](https://github.com/mattwparas/helix). Logo by
[@jakenvac](https://github.com/jakenvac).
