use std::error::Error as _;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use helix_core::hashmap;
use helix_view::{theme::Style, Editor};
use tui::text::Span;

use crate::{alt, compositor::Context, job::Callback, key};

use super::prompt::Movement;
use super::{
    directory_content, overlay, picker,
    picker::{PickerAction, PickerKeyHandler},
    Picker, PickerColumn, Prompt, PromptEvent,
};

/// For each row of the explorer: (path to the item, is the path a directory?).
type ExplorerItem = (PathBuf, bool);
/// The data shared by every row: (file explorer root, style for directories).
type ExplorerData = (PathBuf, Style);

type FileExplorer = Picker<ExplorerItem, ExplorerData>;

type KeyHandler = PickerKeyHandler<ExplorerItem, ExplorerData>;

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

/// Rereads `root` and replaces the open explorer with the result, so that a
/// completed file operation becomes visible. The cursor is restored to `cursor`.
fn refresh_file_explorer(cursor: u32, cx: &mut Context, root: PathBuf) {
    let callback = Box::pin(async move {
        let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
            // Replace the old file explorer with a new one. `remove` is used
            // rather than `pop` so that only the picker is ever removed;
            // `Overlay` forwards the picker's id.
            compositor.remove(picker::ID);
            if let Ok(picker) = file_explorer(Some(cursor), root, editor) {
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

/// Creates a new file, or a directory when the typed name ends with a path
/// separator. Missing intermediate directories are created too.
///
/// Asks for confirmation before overwriting an existing path.
pub(super) fn create_file_or_directory(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation { path, root, cursor } = op;

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

                        refresh_file_explorer(cursor, cx, root);

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

                    refresh_file_explorer(cursor, cx, root);

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
    let FileOperation { path, root, cursor } = op;

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

                    refresh_file_explorer(cursor, cx, root);

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
    let FileOperation { path, root, cursor } = op;

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

                refresh_file_explorer(cursor, cx, root);

                return Some(Ok(format!("Deleted directory: {}", to_delete.display())));
            }

            if let Err(err) = fs::remove_file(to_delete) {
                return Some(Err(format!(
                    "Unable to delete file {}: {err}",
                    to_delete.display()
                )));
            }

            refresh_file_explorer(cursor, cx, root);

            Some(Ok(format!("Deleted file: {}", to_delete.display())))
        },
    )
}

/// Copies the selected file. Directories are not supported.
///
/// Asks for confirmation before overwriting an existing path.
pub(super) fn copy_selected(cx: &mut Context, op: FileOperation<'_>) {
    let FileOperation { path, root, cursor } = op;

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

                    refresh_file_explorer(cursor, cx, picker_root);

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
    let directory_style = editor.theme.get("ui.text.directory");
    let directory_content = directory_content(&root, editor)?;

    // The contents may have shrunk since the cursor was captured, for instance
    // because the user just deleted the last entry.
    let cursor = cursor
        .unwrap_or_default()
        .min(directory_content.len().saturating_sub(1) as u32);

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

    let picker = Picker::new(
        columns,
        0,
        directory_content,
        (root, directory_style),
        move |cx, (path, is_dir): &ExplorerItem, action| {
            if *is_dir {
                let new_root = helix_stdx::path::normalize(path);
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |editor, compositor| {
                            if let Ok(picker) = file_explorer(None, new_root, editor) {
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
    .with_preview(|_editor, (path, _is_dir)| Some((path.as_path().into(), None)))
    .with_key_handlers(hashmap! {
        alt!('n') => file_operation_key(create_file_or_directory),
        alt!('m') => file_operation_key(move_selected),
        alt!('r') => file_operation_key(rename_selected),
        alt!('x') => file_operation_key(delete_selected),
        alt!('c') => file_operation_key(copy_selected),
        alt!('y') => file_operation_key(yank_selected_path),
    })
    // The same operations on unmodified keys, live only in the normal mode of
    // a modal picker (`editor.picker.modal`). `d` can be the delete key here
    // because normal mode never types into the query, unlike `Alt-d`, which
    // the prompt owns and which therefore stays spelled `Alt-x` above.
    .with_modal_key_handlers(hashmap! {
        key!('n') => file_operation_key(create_file_or_directory),
        key!('m') => file_operation_key(move_selected),
        key!('r') => file_operation_key(rename_selected),
        key!('d') => file_operation_key(delete_selected),
        key!('c') => file_operation_key(copy_selected),
        key!('y') => file_operation_key(yank_selected_path),
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
}
