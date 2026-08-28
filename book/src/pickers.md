## Using pickers

Helix has a variety of pickers, which are interactive windows used to select various kinds of items. These include a file picker, global search picker, and more. Most pickers are accessed via keybindings in [space mode](./keymap.md#space-mode). Pickers have their own [keymap](./keymap.md#picker) for navigation.

### Filtering Picker Results

Most pickers perform fuzzy matching using [fzf syntax](https://github.com/junegunn/fzf?tab=readme-ov-file#search-syntax). Two exceptions are the global search picker, which uses regex, and the workspace symbol picker, which passes search terms to the language server. Note that OR operations (`|`) are not currently supported.

If a picker shows multiple columns, you may apply the filter to a specific column by prefixing the column name with `%`. Column names can be shortened to any prefix, so `%p`, `%pa` or `%pat` all mean the same as `%path`. For example, a query of `helix %p .toml !lang` in the global search picker searches for the term "helix" within files with paths ending in ".toml" but not including "lang".

You can insert the contents of a [register](./registers.md) using `Ctrl-r` followed by a register name. For example, one could insert the currently selected text using `Ctrl-r`-`.`, or the directory of the current file using `Ctrl-r`-`%` followed by `Ctrl-w` to remove the last path section. The global search picker will use the contents of the [search register](./registers.md#default-registers) if you press `Enter` without typing a filter. For example, pressing `*`-`Space-/`-`Enter` will start a global search for the currently selected text.

### Modal pickers

Picker actions are normally reached with modifier chords, because every
unmodified key types into the query. Setting `modal = true` in the
[`[editor.picker]`](./editor.md#editorpicker-section) section gives pickers a
normal mode instead: `Escape` leaves the query, and unmodified keys such as `j`,
`k` and `Enter` drive the picker until `i` takes you back to editing the query.
The option is off by default, and while it is off `Escape` closes the picker as
it always has. See the [modal picker keys](./keymap.md#modal-pickers).

### File explorer

`Space-e` opens an interactive file explorer for browsing and opening files, rooted at the workspace; `Space-.` opens one rooted at the current buffer's directory. Unlike the file picker, the explorer does not ignore most files by default; its ignore behaviour is configured separately in the [`[editor.file-explorer]`](./editor.md#editorfile-explorer-section) section.

The explorer can also act on the entry under the cursor: `Alt-n` creates a file or directory, `Alt-m` moves one, `Alt-r` renames one, `Alt-x` deletes one, `Alt-c` copies a file and `Alt-y` yanks a path to a register. Deleting, and overwriting an existing path with a move, rename or copy, always ask for confirmation first. See the [file explorer keys](./keymap.md#file-explorer) for the details.

There is also a clipboard: `Alt-t` cuts the selected entry and `Alt-w` copies it, and `Alt-p` pastes whatever is staged into the directory the explorer is showing. Directories are pasted with everything in them. A cut is spent by the paste that carries it out, while a copy stays staged and can be pasted repeatedly. See the [file explorer clipboard](./keymap.md#file-explorer-clipboard).

In a modal picker the explorer puts a yazi-like layout on unmodified keys — `h` and `l` walk up and down the tree, and `a` create, `r` rename, `d` cut, `y` copy, `p` paste, `D` delete, `Y` yank path and `m` move. See the [modal file explorer keys](./keymap.md#file-explorer-modal-pickers).
