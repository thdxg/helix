use std::error::Error as _;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use helix_core::hashmap;
use helix_view::{
    editor::{ClipboardMode, ExplorerClipboard},
    theme::Style,
    Editor,
};
use tui::text::Span;

use crate::{alt, compositor::Context, job::Callback, key};

use super::prompt::Movement;
use super::{
    directory_content, overlay, picker,
    picker::{PickerAction, PickerKeyHandler, PickerMode, PickerModeHandle},
    Picker, PickerColumn, Prompt, PromptEvent,
};

/// For each row of the explorer: (path to the item, is the path a directory?).
type ExplorerItem = (PathBuf, bool);
/// The data shared by every row: (file explorer root, style for directories).
type ExplorerData = (PathBuf, Style);

type FileExplorer = Picker<ExplorerItem, ExplorerData>;

type KeyHandler = PickerKeyHandler<ExplorerItem, ExplorerData>;

/// Where to leave the cursor in a freshly built explorer.
enum ExplorerCursor {
    /// On this row, clamped to the last one. Used when rereading a directory
    /// after a file operation, to put the cursor back where it was.
    Row(u32),
    /// On the entry holding this path, when it is there. Used when moving up the
    /// tree: which row the directory being left sits on is not known until the
    /// parent has been read.
    Entry(PathBuf),
}

/// The outcome of a file operation: `Ok` is reported as a status message, `Err`
/// as an error. `None` means the operation reported nothing itself, either
/// because it was cancelled or because it deferred to a follow-up prompt.
type OpResult = Option<Result<String, String>>;

/// Everything a file operation needs from the explorer it was triggered from.
///
/// Each operation below takes one of these, opens its own prompts and refreshes
/// the explorer itself, so an operation can be driven from anywhere that can
/// name the selected entry — a picker key handler today, and any other binding
/// layer later.
pub(super) struct FileOperation<'a> {
    /// The entry the operation acts on, i.e. the explorer's selected row.
    pub path: &'a Path,
    /// The directory the explorer is showing.
    pub root: PathBuf,
    /// Where the picker's cursor sits, so it can be restored once the explorer
    /// is reread.
    pub cursor: u32,
    /// Which mode the picker is in, so that a user who is driving the explorer
    /// from normal mode stays in normal mode once it is reread.
    pub picker_mode: PickerMode,
}

/// Checks that `path` is an entry inside `root` that destructive operations may
/// act on.
///
/// The explorer inserts a `..` row at the top of the list for navigating
/// upwards. Destructive operations must refuse it: it resolves to the parent of
/// the explorer root, so deleting or moving it would act well outside anything
/// the user can see.
fn check_within_root(root: &Path, path: &Path) -> Result<(), String> {
    if path.components().next_back() == Some(Component::ParentDir) {
        return Err("Cannot operate on the parent directory entry".to_string());
    }

    let root = helix_stdx::path::normalize(root);
    let normalized = helix_stdx::path::normalize(path);

    if normalized == root || !normalized.starts_with(&root) {
        return Err(format!(
            "{} is outside of the file explorer root {}",
            normalized.display(),
            root.display()
        ));
    }

    Ok(())
}

/// Resolves the destination typed into a move, rename, copy or create prompt.
///
/// `~` is expanded, and a relative path is taken relative to the directory that
/// holds `anchor` rather than to the process' working directory, so that typing
/// a bare name next to the entry the prompt names does what it looks like it
/// does.
fn resolve_destination(anchor: &Path, input: &str) -> PathBuf {
    let expanded = helix_stdx::path::expand_tilde(PathBuf::from(input));

    if expanded.is_absolute() {
        return expanded.into_owned();
    }

    match anchor.parent() {
        Some(parent) => parent.join(expanded),
        None => expanded.into_owned(),
    }
}

/// Runs `overwrite`, asking the user to confirm first if `overwriting` already
/// exists.
///
/// The confirmation defaults to "no": anything other than a literal `y` leaves
/// the existing path untouched.
fn confirm_before_overwriting<F>(
    // The path that is about to be written to.
    overwriting: PathBuf,
    // The path that it will be overwritten with.
    overwrite_with: PathBuf,
    cx: &mut Context,
    picker_root: PathBuf,
    overwrite: F,
) -> OpResult
where
    F: Fn(&mut Context, PathBuf, &Path) -> OpResult + Send + 'static,
{
    // Nothing to confirm: the path does not exist, so we can freely write to it.
    if !overwriting.exists() {
        return overwrite(cx, picker_root, &overwrite_with);
    }

    let callback = Box::pin(async move {
        let call: Callback = Callback::EditorCompositor(Box::new(move |_editor, compositor| {
            let prompt = Prompt::new(
                format!(
                    "Path {} already exists. Overwrite? (y/n): ",
                    overwriting.display()
                )
                .into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || input != "y" {
                        return;
                    }

                    if let Some(result) = overwrite(cx, picker_root.clone(), &overwrite_with) {
                        cx.editor.set_result(result);
                    }
                },
            );

            compositor.push(Box::new(prompt));
        }));
        Ok(call)
    });
    cx.jobs.callback(callback);

    None
}

/// Opens a prompt for one of the explorer's file operations.
fn create_file_operation_prompt<F>(
    cx: &mut Context,
    // The currently selected path of the picker.
    path: &Path,
    // The text of the prompt.
    prompt: fn(&Path) -> String,
    // Where to put the cursor within the prefilled input.
    movement: Option<Movement>,
    // What to prefill the user's input with.
    prefill: fn(&Path) -> String,
    // The operation to run once the user validates the prompt.
    file_op: F,
) where
    F: Fn(&mut Context, &Path, String) -> OpResult + Send + 'static,
{
    let selected_path = path.to_path_buf();
    let callback = Box::pin(async move {
        let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
            // A second copy so that `selected_path` can still be used for the prefill.
            let path = selected_path.clone();
            let mut prompt = Prompt::new(
                prompt(&path).into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    if let Some(result) = file_op(cx, &path, input.to_owned()) {
                        cx.editor.set_result(result);
                    } else {
                        cx.editor.clear_status();
                    }
                },
            );

            prompt.set_line(prefill(&selected_path), editor);

            if let Some(movement) = movement {
                prompt.move_cursor(movement);
            }

            compositor.push(Box::new(prompt));
        }));
        Ok(call)
    });
    cx.jobs.callback(callback);
}

/// Reads `root` and replaces the open explorer with the result, so that a
/// completed file operation becomes visible, or so that the explorer moves to
/// another directory. The cursor lands on row `cursor` and the picker reopens in
/// `picker_mode`.
fn refresh_file_explorer(cursor: u32, picker_mode: PickerMode, cx: &mut Context, root: PathBuf) {
    open_file_explorer(ExplorerCursor::Row(cursor), picker_mode, cx, root)
}

/// The general form of [`refresh_file_explorer`], for when the row to land on is
/// only known once `root` has been read.
fn open_file_explorer(
    cursor: ExplorerCursor,
    picker_mode: PickerMode,
    cx: &mut Context,
    root: PathBuf,
) {
    let callback = Box::pin(async move {
        let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
            // Replace the old file explorer with a new one. `remove` is used
            // rather than `pop` so that only the picker is ever removed;
            // `Overlay` forwards the picker's id.
            compositor.remove(picker::ID);
            if let Ok(picker) = file_explorer_with_mode(cursor, picker_mode, root, editor) {
                compositor.push(Box::new(overlay::overlaid(picker)));
            }
        }));
        Ok(call)
    });
    cx.jobs.callback(callback);
}

/// Puts the cursor just before the file extension of `path`, which is where a
/// rename usually wants to start: the extension normally stays as it is.
fn before_extension(path: &Path) -> Option<Movement> {
    path.extension()
        // +1 to account for the dot in the extension
        .map(|ext| Movement::BackwardChar(ext.len() + 1))
}

/// Yanks the path of the selected entry to a register.
pub(super) fn yank_selected_path(cx: &mut Context, op: FileOperation<'_>) {
    let register = cx
        .editor
        .selected_register
        .unwrap_or(cx.editor.config().default_yank_register);
    let path = helix_stdx::path::get_relative_path(op.path);
    let path = path.to_string_lossy().to_string();
    let message = format!("Yanked path {} to register {register}", path);

    match cx.editor.registers.write(register, vec![path]) {
        Ok(()) => cx.editor.set_status(message),
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

/// Moves the explorer to the parent of the directory it is showing: the `h` of
/// yazi's `h`/`l` tree navigation, and the same move as pressing `Enter` on the
/// `..` row.
///
/// The cursor lands on the directory just left, so that `h` and `l` retrace each
/// other's steps.
pub(super) fn parent_directory(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation {
        root, picker_mode, ..
    } = op;

    // Spelled the way the `..` row is, so that the two agree about where up is.
    let parent = helix_stdx::path::normalize(root.join(".."));
    if parent == root {
        cx.editor
            .set_status("Already at the root of the filesystem");
        return;
    }

    // Handed over as a path rather than resolved to a row here: the parent has to
    // be read to find the row, and the rebuild is about to read it anyway. That
    // matters — a directory with thousands of entries takes a noticeable moment
    // to walk, and walking it twice per keypress would be felt.
    open_file_explorer(ExplorerCursor::Entry(root), picker_mode, cx, parent);
}

/// Creates a new file, or a directory when the typed name ends with a path
/// separator. Missing intermediate directories are created too.
///
/// Asks for confirmation before overwriting an existing path.
pub(super) fn create_file_or_directory(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation {
        path,
        root,
        cursor,
        picker_mode,
    } = op;

    create_file_operation_prompt(
        cx,
        path,
        |_| "Create: ".into(),
        None,
        |path| {
            path.parent()
                .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
                .unwrap_or_default()
        },
        move |cx, selected, to_create_string| {
            let root = root.clone();
            let to_create = resolve_destination(selected, &to_create_string);

            confirm_before_overwriting(
                to_create.clone(),
                to_create,
                cx,
                root,
                move |cx: &mut Context, root: PathBuf, to_create: &Path| {
                    if to_create_string.ends_with(std::path::MAIN_SEPARATOR) {
                        if let Err(err) = fs::create_dir_all(to_create) {
                            return Some(Err(format!(
                                "Unable to create directory {}: {err}",
                                to_create.display()
                            )));
                        }

                        refresh_file_explorer(cursor, picker_mode, cx, root);

                        return Some(Ok(format!("Created directory: {}", to_create.display())));
                    }

                    // Allows creating a path like /path/to/somewhere.txt even if
                    // "to" does not exist yet.
                    let Some(to_create_parent) = to_create.parent() else {
                        return Some(Err(format!(
                            "Failed to get parent directory of {}",
                            to_create.display()
                        )));
                    };

                    if let Err(err) = fs::create_dir_all(to_create_parent) {
                        return Some(Err(format!(
                            "Could not create intermediate directories: {err}"
                        )));
                    }

                    if let Err(err) = fs::File::create(to_create) {
                        return Some(Err(format!(
                            "Unable to create file {}: {err}",
                            to_create.display()
                        )));
                    }

                    refresh_file_explorer(cursor, picker_mode, cx, root);

                    Some(Ok(format!("Created file: {}", to_create.display())))
                },
            )
        },
    )
}

/// Shared implementation of [`move_selected`] and [`rename_selected`]: the two
/// differ only in what the prompt says and what it is prefilled with.
///
/// Asks for confirmation before overwriting an existing path.
fn move_selected_with(
    cx: &mut Context,
    op: FileOperation<'_>,
    prompt: fn(&Path) -> String,
    prefill: fn(&Path) -> String,
) {
    let FileOperation {
        path,
        root,
        cursor,
        picker_mode,
    } = op;

    if let Err(err) = check_within_root(&root, path) {
        cx.editor.set_error(err);
        return;
    }

    create_file_operation_prompt(
        cx,
        path,
        prompt,
        before_extension(path),
        prefill,
        move |cx, move_from, move_to_string| {
            let root = root.clone();
            let move_to = resolve_destination(move_from, &move_to_string);

            confirm_before_overwriting(
                move_to,
                move_from.to_path_buf(),
                cx,
                root,
                move |cx: &mut Context, root: PathBuf, move_from: &Path| {
                    let move_to = resolve_destination(move_from, &move_to_string);

                    if let Err(err) = cx.editor.move_path(move_from, &move_to) {
                        return Some(Err(format!(
                            "Unable to move {} {} -> {}: {err}",
                            if move_to_string.ends_with(std::path::MAIN_SEPARATOR) {
                                "directory"
                            } else {
                                "file"
                            },
                            move_from.display(),
                            move_to.display()
                        )));
                    }

                    refresh_file_explorer(cursor, picker_mode, cx, root);

                    Some(Ok(format!(
                        "Moved {} -> {}",
                        move_from.display(),
                        move_to.display()
                    )))
                },
            )
        },
    )
}

/// Moves the selected file or directory somewhere else. The prompt is prefilled
/// with the entry's full path.
pub(super) fn move_selected(cx: &mut Context, op: FileOperation<'_>) {
    move_selected_with(
        cx,
        op,
        |path| format!("Move {} -> ", path.display()),
        |path| path.display().to_string(),
    )
}

/// Renames the selected file or directory in place. The prompt is prefilled with
/// just the entry's name; a relative name is resolved next to the entry.
pub(super) fn rename_selected(cx: &mut Context, op: FileOperation<'_>) {
    move_selected_with(
        cx,
        op,
        |path| {
            format!(
                "Rename {} -> ",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        },
        |path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into()
        },
    )
}

/// Deletes the selected file, or the selected directory and everything in it.
///
/// Always asks for confirmation first, and the confirmation defaults to "no".
pub(super) fn delete_selected(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation {
        path,
        root,
        cursor,
        picker_mode,
    } = op;

    if let Err(err) = check_within_root(&root, path) {
        cx.editor.set_error(err);
        return;
    }

    create_file_operation_prompt(
        cx,
        path,
        |path| {
            if path.is_dir() {
                format!("Delete {} and everything in it? (y/n): ", path.display())
            } else {
                format!("Delete {}? (y/n): ", path.display())
            }
        },
        None,
        |_| String::new(),
        move |cx, to_delete, confirmation| {
            let root = root.clone();

            // Anything but an explicit "y" cancels the deletion.
            if confirmation != "y" {
                return None;
            }

            // Checked again here rather than trusting the earlier check: this
            // runs later, from a different layer, and deleting the wrong tree is
            // not recoverable.
            if let Err(err) = check_within_root(&root, to_delete) {
                return Some(Err(err));
            }

            // `symlink_metadata` so that a symlink to a directory is removed as a
            // link instead of being followed into the directory it points at.
            let metadata = match fs::symlink_metadata(to_delete) {
                Ok(metadata) => metadata,
                Err(err) => {
                    return Some(Err(format!(
                        "Unable to read {}: {err}",
                        to_delete.display()
                    )))
                }
            };

            if metadata.is_dir() {
                if let Err(err) = fs::remove_dir_all(to_delete) {
                    return Some(Err(format!(
                        "Unable to delete directory {}: {err}",
                        to_delete.display()
                    )));
                }

                refresh_file_explorer(cursor, picker_mode, cx, root);

                return Some(Ok(format!("Deleted directory: {}", to_delete.display())));
            }

            if let Err(err) = fs::remove_file(to_delete) {
                return Some(Err(format!(
                    "Unable to delete file {}: {err}",
                    to_delete.display()
                )));
            }

            refresh_file_explorer(cursor, picker_mode, cx, root);

            Some(Ok(format!("Deleted file: {}", to_delete.display())))
        },
    )
}

/// Copies the selected file. Directories are not supported.
///
/// Asks for confirmation before overwriting an existing path.
pub(super) fn copy_selected(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation {
        path,
        root,
        cursor,
        picker_mode,
    } = op;

    create_file_operation_prompt(
        cx,
        path,
        |path| format!("Copy {} -> ", path.display()),
        None,
        |path| {
            path.parent()
                .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
                .unwrap_or_default()
        },
        move |cx, copy_from, copy_to_string| {
            let root = root.clone();

            if copy_from.is_dir() || copy_to_string.ends_with(std::path::MAIN_SEPARATOR) {
                // TODO: support copying directories recursively. This isn't built
                // in to the standard library.
                return Some(Err(format!(
                    "Copying directories is not supported: {} is a directory",
                    copy_from.display()
                )));
            }

            let copy_to = resolve_destination(copy_from, &copy_to_string);

            confirm_before_overwriting(
                copy_to,
                copy_from.to_path_buf(),
                cx,
                root,
                move |cx: &mut Context, picker_root: PathBuf, copy_from: &Path| {
                    let copy_to = resolve_destination(copy_from, &copy_to_string);

                    if let Err(err) = fs::copy(copy_from, &copy_to) {
                        return Some(Err(format!(
                            "Unable to copy from file {} to {}: {err}",
                            copy_from.display(),
                            copy_to.display()
                        )));
                    }

                    refresh_file_explorer(cursor, picker_mode, cx, picker_root);

                    Some(Ok(format!(
                        "Copied contents of file {} to {}",
                        copy_from.display(),
                        copy_to.display()
                    )))
                },
            )
        },
    )
}

/// How a staged set of paths is named in the status line: the entry's own name
/// when there is one of it, a count otherwise.
fn staged_description(paths: &[PathBuf]) -> String {
    match paths {
        [path] => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        paths => format!("{} entries", paths.len()),
    }
}

/// Shared implementation of [`cut_selected`] and [`copy_selected_to_clipboard`]:
/// puts the selected entry on the editor's explorer clipboard, to be pasted by
/// [`paste_clipboard`] into whichever directory the explorer is showing then.
fn stage_on_clipboard(cx: &mut Context, op: FileOperation<'_>, mode: ClipboardMode) {
    let FileOperation { path, root, .. } = op;

    // Staging the `..` row, the root, or anything outside it is refused here so
    // that a later paste can never be handed a path the explorer does not own.
    if let Err(err) = check_within_root(&root, path) {
        cx.editor.set_error(err);
        return;
    }

    let paths = vec![helix_stdx::path::normalize(path)];
    let description = staged_description(&paths);

    cx.editor.explorer_clipboard = Some(ExplorerClipboard { paths, mode });
    cx.editor.set_status(match mode {
        ClipboardMode::Cut => format!("Cut {description}"),
        ClipboardMode::Copy => format!("Copied {description}"),
    });
}

/// Stages the selected entry to be moved by the next paste.
pub(super) fn cut_selected(cx: &mut Context, op: FileOperation<'_>) {
    stage_on_clipboard(cx, op, ClipboardMode::Cut)
}

/// Stages the selected entry to be duplicated by the next paste.
pub(super) fn copy_selected_to_clipboard(cx: &mut Context, op: FileOperation<'_>) {
    stage_on_clipboard(cx, op, ClipboardMode::Copy)
}

/// Works out where `source` lands when pasted into `root`, rejecting anything
/// that would write outside the explorer or recurse for ever.
fn paste_destination(root: &Path, source: &Path) -> Result<PathBuf, String> {
    let Some(name) = source.file_name() else {
        return Err(format!(
            "{} has no file name to paste under",
            source.display()
        ));
    };

    let source = helix_stdx::path::normalize(source);
    let destination = helix_stdx::path::normalize(root.join(name));

    // A paste must land inside the directory the explorer is showing, the same
    // guard the destructive operations use on their target.
    check_within_root(root, &destination)?;

    if destination == source {
        return Err(format!(
            "{} is already in {}",
            name.to_string_lossy(),
            root.display()
        ));
    }

    // Pasting a directory into itself, or into one of its own descendants,
    // would copy the copy for ever.
    if destination.starts_with(&source) {
        return Err(format!("Cannot paste {} into itself", source.display()));
    }

    Ok(destination)
}

/// Drops `source` from the clipboard once a paste has carried it out, clearing
/// the clipboard entirely once nothing is left staged. See
/// [`ExplorerClipboard::consume`].
fn consume_pasted_entry(editor: &mut Editor, source: &Path) {
    let Some(clipboard) = editor.explorer_clipboard.as_mut() else {
        return;
    };

    if !clipboard.consume(source) {
        editor.explorer_clipboard = None;
    }
}

/// Carries out a single staged paste, once any overwrite has been confirmed.
fn paste_entry(
    cx: &mut Context,
    root: PathBuf,
    source: &Path,
    destination: &Path,
    mode: ClipboardMode,
    cursor: u32,
    picker_mode: PickerMode,
) -> OpResult {
    // Checked again rather than trusting the check made when the paste started:
    // a confirmation may have run in between, from a different layer.
    if let Err(err) = check_within_root(&root, destination) {
        return Some(Err(err));
    }

    // The entry may have been moved or deleted since it was staged.
    if let Err(err) = fs::symlink_metadata(source) {
        return Some(Err(format!("Unable to read {}: {err}", source.display())));
    }

    let outcome = match mode {
        // `move_path` renames, tells the language servers about it and follows
        // any open document to its new path. It falls back to a copy followed by
        // a delete when the two paths are on different filesystems.
        ClipboardMode::Cut => cx.editor.move_path(source, destination).map_err(|err| {
            format!(
                "Unable to move {} -> {}: {err}",
                source.display(),
                destination.display()
            )
        }),
        // Recursive, so that pasting a directory brings its contents along.
        ClipboardMode::Copy => helix_stdx::path::copy_all(source, destination).map_err(|err| {
            format!(
                "Unable to copy {} -> {}: {err}",
                source.display(),
                destination.display()
            )
        }),
    };

    if let Err(err) = outcome {
        return Some(Err(err));
    }

    consume_pasted_entry(cx.editor, source);

    let verb = match mode {
        ClipboardMode::Cut => "Moved",
        ClipboardMode::Copy => "Copied",
    };

    refresh_file_explorer(cursor, picker_mode, cx, root);

    Some(Ok(format!(
        "{verb} {} -> {}",
        source.display(),
        destination.display()
    )))
}

/// Pastes whatever [`cut_selected`] or [`copy_selected_to_clipboard`] staged
/// into the directory the explorer is showing.
///
/// The destination is the explorer's root rather than the entry under the
/// cursor, matching how yazi's paste behaves. Overwriting an existing entry is
/// confirmed first, and the confirmation defaults to "no".
pub(super) fn paste_clipboard(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation {
        root,
        cursor,
        picker_mode,
        ..
    } = op;

    let Some(clipboard) = cx.editor.explorer_clipboard.clone() else {
        cx.editor
            .set_status("Nothing staged: cut or copy an entry before pasting");
        return;
    };

    // Every destination is resolved and vetted before anything is written, so
    // that a paste which cannot work leaves the filesystem untouched.
    let mut entries = Vec::with_capacity(clipboard.paths.len());
    for source in &clipboard.paths {
        match paste_destination(&root, source) {
            Ok(destination) => entries.push((source.clone(), destination)),
            Err(err) => {
                cx.editor.set_error(err);
                return;
            }
        }
    }

    // One entry at a time. The clipboard only ever holds one today; once a
    // multi-select fills it, several colliding entries would stack one
    // confirmation prompt each.
    for (source, destination) in entries {
        let result = confirm_before_overwriting(
            destination.clone(),
            source,
            cx,
            root.clone(),
            move |cx: &mut Context, root: PathBuf, source: &Path| {
                paste_entry(
                    cx,
                    root,
                    source,
                    &destination,
                    clipboard.mode,
                    cursor,
                    picker_mode,
                )
            },
        );

        if let Some(result) = result {
            cx.editor.set_result(result);
        }
    }
}

/// Wraps one of the file operations above into a picker key handler.
fn file_operation_key(operation: fn(&mut Context, FileOperation<'_>)) -> KeyHandler {
    Box::new(
        move |cx, args: PickerAction<'_, ExplorerItem, ExplorerData>| {
            let (path, _is_dir) = args.selection;
            operation(
                cx,
                FileOperation {
                    path,
                    root: args.data.0.clone(),
                    cursor: args.cursor,
                    picker_mode: args.mode,
                },
            )
        },
    )
}

/// Builds the file explorer picker rooted at `root`.
///
/// `cursor` restores the position of the picker's cursor, for when the explorer
/// is reopened after a file operation.
pub fn file_explorer(
    cursor: Option<u32>,
    root: PathBuf,
    editor: &Editor,
) -> Result<FileExplorer, std::io::Error> {
    file_explorer_with_mode(
        ExplorerCursor::Row(cursor.unwrap_or_default()),
        PickerMode::default(),
        root,
        editor,
    )
}

/// As [`file_explorer`], but opening in `mode`.
///
/// The explorer does not update itself in place: a file operation and a descent
/// into a subdirectory both throw the picker away and build a fresh one. Left to
/// itself the replacement would open in [`PickerMode::Insert`] like any other
/// picker, dropping a user who was driving the explorer with bare keys back into
/// the query without having asked to be — so the mode is carried across the
/// rebuild.
fn file_explorer_with_mode(
    cursor: ExplorerCursor,
    mode: PickerMode,
    root: PathBuf,
    editor: &Editor,
) -> Result<FileExplorer, std::io::Error> {
    let directory_style = editor.theme.get("ui.text.directory");
    let directory_content = directory_content(&root, editor)?;

    let last_row = directory_content.len().saturating_sub(1) as u32;
    let cursor = match cursor {
        // The contents may have shrunk since the row was captured, for instance
        // because the user just deleted the last entry.
        ExplorerCursor::Row(row) => row.min(last_row),
        // `flatten_dirs` collapses a chain of single-child directories into one
        // row, so the row for the directory being left may name a path above or
        // below it rather than exactly it. Matching on either containing the
        // other covers both; `Path::starts_with` works a component at a time, so
        // it cannot confuse `foo` with a sibling `foobar`.
        ExplorerCursor::Entry(entry) => directory_content
            .iter()
            .position(|(path, is_dir)| {
                *is_dir && (entry.starts_with(path) || path.starts_with(&entry))
            })
            .unwrap_or_default() as u32,
    };

    let columns = [PickerColumn::new(
        "path",
        |(path, is_dir): &ExplorerItem, (root, directory_style): &ExplorerData| {
            let name = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
            if *is_dir {
                Span::styled(format!("{}/", name), *directory_style).into()
            } else {
                name.into()
            }
        },
    )];

    // Shared with the picker below, so that descending into a directory can read
    // the mode the user is in at the moment they press Enter — which is not
    // necessarily `mode`, since `Esc` and `i` move between modes in between.
    let mode = PickerModeHandle::new(mode);
    let descend_mode = mode.clone();

    let picker = Picker::new(
        columns,
        0,
        directory_content,
        (root, directory_style),
        move |cx, (path, is_dir): &ExplorerItem, action| {
            if *is_dir {
                let new_root = helix_stdx::path::normalize(path);
                let mode = descend_mode.get();
                let cursor = ExplorerCursor::Row(0);
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |editor, compositor| {
                            if let Ok(picker) =
                                file_explorer_with_mode(cursor, mode, new_root, editor)
                            {
                                compositor.push(Box::new(overlay::overlaid(picker)));
                            }
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
            } else if let Err(e) = cx.editor.open(path, action) {
                let err = if let Some(err) = e.source() {
                    format!("{}", err)
                } else {
                    format!("unable to open \"{}\"", path.display())
                };
                cx.editor.set_error(err);
            }
        },
    )
    .with_initial_cursor(cursor)
    .with_mode_handle(mode)
    .with_preview(|_editor, (path, _is_dir)| Some((path.as_path().into(), None)))
    .with_key_handlers(hashmap! {
        alt!('n') => file_operation_key(create_file_or_directory),
        alt!('m') => file_operation_key(move_selected),
        alt!('r') => file_operation_key(rename_selected),
        alt!('x') => file_operation_key(delete_selected),
        alt!('c') => file_operation_key(copy_selected),
        alt!('y') => file_operation_key(yank_selected_path),
        // The clipboard trio. `Alt-d` and `Alt-y`, which would have matched the
        // bare keys below, are already spoken for — the prompt owns `Alt-d` and
        // `Alt-y` yanks the selected path — so the cut and copy keys are
        // `Alt-t` (take) and `Alt-w`, the latter after Emacs' `M-w` for copy.
        alt!('t') => file_operation_key(cut_selected),
        alt!('w') => file_operation_key(copy_selected_to_clipboard),
        alt!('p') => file_operation_key(paste_clipboard),
    })
    // A yazi-like layout on unmodified keys, live only in the normal mode of a
    // modal picker (`editor.picker.modal`). Normal mode never types into the
    // query, so plain letters are free here in a way they are not above.
    //
    // Unlike yazi, `d` cuts and `D` deletes: there is no trash to send an entry
    // to, so the shifted key takes the unrecoverable operation. `c` is left
    // unbound, reserved for a yazi-style `c`-prefixed family later on.
    //
    // `a` shadows one of the picker's three ways into insert mode; `i` and `/`
    // still get to the query.
    .with_modal_key_handlers(hashmap! {
        key!('h') => file_operation_key(parent_directory),
        key!('a') => file_operation_key(create_file_or_directory),
        key!('r') => file_operation_key(rename_selected),
        key!('m') => file_operation_key(move_selected),
        key!('d') => file_operation_key(cut_selected),
        key!('y') => file_operation_key(copy_selected_to_clipboard),
        key!('p') => file_operation_key(paste_clipboard),
        key!('D') => file_operation_key(delete_selected),
        key!('Y') => file_operation_key(yank_selected_path),
    });

    Ok(picker)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_check_within_root() {
        let root = Path::new("/home/user/project");

        // Ordinary entries of the explorer are fine.
        assert!(check_within_root(root, Path::new("/home/user/project/src")).is_ok());
        assert!(check_within_root(root, Path::new("/home/user/project/README.md")).is_ok());
        // `flatten_dirs` can produce entries nested deeper than one level.
        assert!(check_within_root(root, Path::new("/home/user/project/a/b/c")).is_ok());

        // The `..` row the explorer inserts must never be operated on.
        assert!(check_within_root(root, Path::new("/home/user/project/..")).is_err());
        // Neither may the root itself, nor anything outside it.
        assert!(check_within_root(root, root).is_err());
        assert!(check_within_root(root, Path::new("/home/user/other")).is_err());
        assert!(check_within_root(root, Path::new("/home/user/project/../other")).is_err());
        // A sibling whose name merely starts with the root's name is outside it.
        assert!(check_within_root(root, Path::new("/home/user/project2/src")).is_err());
    }

    #[test]
    fn test_resolve_destination() {
        let anchor = Path::new("/home/user/project/src/main.rs");

        // A bare name lands next to the entry the prompt named, not in the
        // process' working directory.
        assert_eq!(
            resolve_destination(anchor, "lib.rs"),
            PathBuf::from("/home/user/project/src/lib.rs")
        );
        // An absolute destination is taken as given.
        assert_eq!(
            resolve_destination(anchor, "/tmp/lib.rs"),
            PathBuf::from("/tmp/lib.rs")
        );
    }

    #[test]
    fn test_paste_destination() {
        let root = Path::new("/home/user/project");

        // A paste lands under the directory the explorer is showing, keeping
        // the staged entry's own name.
        assert_eq!(
            paste_destination(root, Path::new("/tmp/notes.md")),
            Ok(PathBuf::from("/home/user/project/notes.md"))
        );
        assert_eq!(
            paste_destination(root, Path::new("/tmp/src")),
            Ok(PathBuf::from("/home/user/project/src"))
        );

        // Pasting an entry back into the directory it already lives in is a
        // no-op at best and a self-overwrite at worst.
        assert!(paste_destination(root, Path::new("/home/user/project/src")).is_err());

        // Pasting a directory into itself or into one of its descendants would
        // recurse for ever.
        assert!(paste_destination(Path::new("/tmp/src"), Path::new("/tmp/src")).is_err());
        assert!(paste_destination(Path::new("/tmp/src/nested"), Path::new("/tmp/src")).is_err());

        // A staged path with no file name has nowhere to land.
        assert!(paste_destination(root, Path::new("/")).is_err());
    }

    #[test]
    fn test_staged_description() {
        assert_eq!(
            staged_description(&[PathBuf::from("/home/user/project/main.rs")]),
            "main.rs"
        );
        assert_eq!(
            staged_description(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            "2 entries"
        );
    }
}
