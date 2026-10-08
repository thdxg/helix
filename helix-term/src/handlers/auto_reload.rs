use std::borrow::Cow;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{self, AtomicBool, AtomicUsize};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use helix_core::file_watcher::{events_from_paths, EventType, FileSystemDidChange};
use helix_event::{dispatch, register_hook, send_blocking};
use helix_view::editor::Config;
use helix_view::events::ConfigDidChange;
use helix_view::handlers::{AutoReloadEvent, Handlers};
use helix_view::{DocumentId, Editor};
use tokio::time::Instant;

use crate::compositor::Compositor;
use crate::ui::{Prompt, PromptEvent};
use crate::{job, ui};

/// Handler for FileSystemDidChange events (from filesentry or polling)
struct ReloadHandler {
    enable: AtomicBool,
    prompt_if_modified: AtomicBool,
}

impl ReloadHandler {
    pub fn refresh_config(&self, config: &Config) {
        self.enable
            .store(config.auto_reload.enable, atomic::Ordering::Relaxed);
        self.prompt_if_modified.store(
            config.auto_reload.prompt_if_modified,
            atomic::Ordering::Relaxed,
        );
    }

    fn on_file_did_change(&self, event: &mut FileSystemDidChange) {
        if !self.enable.load(atomic::Ordering::Relaxed) {
            return;
        }
        let fs_events = event.fs_events.clone();
        // Tempfiles (created and removed within the settle period) never matter. Any
        // other event may move a VCS ref -- a new branch's first commit creates its
        // loose ref -- which `needs_reload` decides on the main thread below.
        if fs_events
            .iter()
            .all(|event| event.ty == EventType::Tempfile)
        {
            return;
        }
        let prompt_if_modified = self.prompt_if_modified.load(atomic::Ordering::Relaxed);
        job::dispatch_blocking(move |editor, compositor| {
            let mut vcs_reload = false;

            for fs_event in &*fs_events {
                vcs_reload |= editor.diff_providers.needs_reload(fs_event);

                // React to content changes (Modified) and to deletions (Delete) of
                // open files. Create carries no action for already-open buffers.
                if !matches!(fs_event.ty, EventType::Modified | EventType::Delete) {
                    continue;
                }
                let Some(doc_id) = editor.document_id_by_path(fs_event.path.as_std_path()) else {
                    continue;
                };

                handle_document_change(editor, compositor, doc_id, prompt_if_modified);
            }

            if vcs_reload {
                reload_vcs_diffs(editor);
            }
        });
    }
}

/// Handler for polling-based change detection for unwatched files.
/// Polls documents not covered by the file watcher (e.g., outside workspace or in ignored dirs).
/// Co-Authored-By: Anthony Rubick <68485672+AnthonyMichaelTDM@users.noreply.github.com>
#[derive(Debug)]
pub(super) struct PollHandler;

impl PollHandler {
    pub fn new() -> Self {
        PollHandler
    }
}

impl helix_event::AsyncHook for PollHandler {
    type Event = AutoReloadEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _existing_debounce: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            AutoReloadEvent::PollAfter { interval } => {
                Some(Instant::now() + Duration::from_millis(interval))
            }
        }
    }

    fn finish_debounce(&mut self) {
        job::dispatch_blocking(move |editor, _compositor| {
            let config = editor.config();
            if !config.auto_reload.enable || !config.auto_reload.poll.enable {
                return;
            }

            let poll_interval = config.auto_reload.poll.interval;

            // Check unwatched documents for external modifications
            let modified_paths = changed_unwatched_paths(editor);

            // Poll extra watched paths (e.g., VCS HEAD files outside workspace)
            let extra_path_changes = editor.file_watcher.poll_extra_paths();

            // Dispatch changes through the FileSystemDidChange hook
            let all_changed: Vec<PathBuf> = modified_paths
                .into_iter()
                .chain(extra_path_changes)
                .collect();
            if !all_changed.is_empty() {
                let events = events_from_paths(all_changed);
                dispatch(FileSystemDidChange { fs_events: events });
            }

            // Schedule next poll
            send_blocking(
                &editor.handlers.auto_reload,
                AutoReloadEvent::PollAfter {
                    interval: poll_interval,
                },
            );
        });
    }
}

/// Open documents not covered by the file watcher (outside the workspace, or
/// ignored) whose on-disk mtime no longer matches their last save.
fn changed_unwatched_paths(editor: &Editor) -> Vec<PathBuf> {
    editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?;
            if editor.file_watcher.is_watching(path) {
                return None;
            }
            let mtime = path.metadata().ok()?.modified().ok()?;
            (mtime != doc.last_saved_time).then(|| path.to_path_buf())
        })
        .collect()
}

/// Re-check unwatched open files and VCS state when the terminal regains focus --
/// they are only polled otherwise, and regaining focus is when a user is most likely
/// to have just edited or committed them elsewhere. Routes through the same
/// `FileSystemDidChange` path as the watcher and poll (so prompt de-duplication
/// still applies).
pub(crate) fn on_focus_gained(editor: &mut Editor) {
    if !editor.config().auto_reload.enable {
        return;
    }
    let mut changed = changed_unwatched_paths(editor);
    changed.extend(editor.file_watcher.poll_extra_paths());
    if !changed.is_empty() {
        dispatch(FileSystemDidChange {
            fs_events: events_from_paths(changed),
        });
    }
}

/// Handler for document changes detected by filesentry or polling
fn handle_document_change(
    editor: &mut Editor,
    compositor: &mut Compositor,
    doc_id: DocumentId,
    prompt_if_modified: bool,
) {
    let scrolloff = editor.config().scrolloff;
    let target_view_id = editor.get_synced_view_id(doc_id);

    // Compute the workspace-trust decision up front (needs an immutable borrow of the
    // editor) so it can be passed to `doc.reload` below without conflicting with the
    // mutable document/view borrows.
    let trust_full = {
        let doc = doc!(editor, &doc_id);
        editor
            .workspace_trust
            .query(
                doc.workspace_root(),
                helix_loader::workspace_trust::TrustQuery::Git,
            )
            .is_trusted()
    };

    let doc = doc_mut!(editor, &doc_id);
    let Some(path) = doc.path().map(|p| p.to_path_buf()) else {
        return;
    };

    let mtime = match path.metadata() {
        Ok(meta) => meta.modified().unwrap_or(SystemTime::now()),
        // The file is gone. Statting it here (rather than trusting the event type) is
        // also the atomic-save guard: a write-to-temp+rename briefly deletes the target
        // but has already recreated it by the time we look, so that path returns `Ok`
        // above and reloads normally -- only a genuine deletion reaches here.
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            handle_document_deleted(editor, doc_id);
            return;
        }
        // On a transient error (permissions, flaky mount) skip this tick rather than
        // fabricate a fresh `now()` mtime, which would never match `last_saved_time`
        // and would re-trigger a false "changed externally" prompt on every poll.
        Err(_) => return,
    };

    if mtime == doc.last_saved_time {
        return;
    }

    if doc.is_modified() {
        // Surface a conflict (prompt / warning) once per distinct on-disk mtime;
        // the buffer is modified so we can't reload, and the poll / focus checks
        // would otherwise re-fire it on every tick.
        if doc.auto_reload_seen_mtime == Some(mtime) {
            return;
        }
        doc.auto_reload_seen_mtime = Some(mtime);
        if prompt_if_modified {
            let path_str = doc
                .relative_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[scratch]".into());
            prompt_reload_modified(compositor, doc_id, path_str);
        } else {
            let msg = format!(
                "{} changed externally but has unsaved changes, use :reload to refresh",
                doc.relative_path().unwrap().display()
            );
            editor.set_warning(msg);
        }
    } else {
        let view = view_mut!(editor, target_view_id);
        match doc.reload(view, &editor.diff_providers, trust_full) {
            Ok(_) => {
                view.ensure_cursor_in_view(doc, scrolloff);
                let msg = format!(
                    "{} reloaded (external changes)",
                    doc.relative_path().unwrap().display()
                );
                editor.set_status(msg);
            }
            Err(err) => {
                let doc = doc!(editor, &doc_id);
                let msg = format!(
                    "{} auto-reload failed: {err}",
                    doc.relative_path().unwrap().display()
                );
                editor.set_error(msg);
            }
        }
    }
}

/// The backing file of an open document was deleted externally. Keep the buffer and
/// its in-memory content, but flag it so it counts as modified: `:w` recreates the
/// file, `:q` warns about unsaved changes, and `:reload` fails gracefully (the buffer
/// is preserved). The view is never switched. Warns once -- the flag both drives the
/// modified state and de-duplicates the notification across repeated poll/watch ticks.
fn handle_document_deleted(editor: &mut Editor, doc_id: DocumentId) {
    let doc = doc_mut!(editor, &doc_id);
    if doc.is_deleted_from_disk() {
        return;
    }
    doc.set_deleted_from_disk(true);
    // Reset so that if the file is recreated later, `handle_document_change` sees a
    // fresh mtime and offers a normal reload instead of being suppressed by the guard.
    doc.auto_reload_seen_mtime = None;
    let name = doc
        .relative_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[scratch]".into());
    editor.set_warning(format!(
        "{name} was deleted on disk; buffer kept (:w to save, :bc! to discard)"
    ));
}

/// Bumped by every [`reload_vcs_diffs`], so that of several overlapping reloads (a
/// rebase moves `HEAD` once per commit) only the newest is applied.
static VCS_RELOAD_GENERATION: AtomicUsize = AtomicUsize::new(0);

/// Re-read the diff base and head name of every document after `HEAD` moved.
///
/// Reading them opens the repository and walks the commit tree once per document,
/// so this runs off the main thread and applies the results in a job.
fn reload_vcs_diffs(editor: &mut Editor) {
    let generation = VCS_RELOAD_GENERATION.fetch_add(1, atomic::Ordering::Relaxed) + 1;
    let docs: Vec<(DocumentId, PathBuf, bool)> = editor
        .documents
        .values()
        .filter_map(|doc| {
            let path = doc.path()?.to_path_buf();
            let trust_full = editor
                .workspace_trust
                .query(
                    doc.workspace_root(),
                    helix_loader::workspace_trust::TrustQuery::Git,
                )
                .is_trusted();
            Some((doc.id(), path, trust_full))
        })
        .collect();
    let diff_providers = editor.diff_providers.clone();
    let (workspace, _) = helix_loader::find_workspace();

    tokio::task::spawn_blocking(move || {
        let updates: Vec<_> = docs
            .into_iter()
            .map(|(doc_id, path, trust_full)| {
                let diff_base = diff_providers.get_diff_base(&path, trust_full);
                let head = diff_providers.get_current_head_name(&path, trust_full);
                (doc_id, diff_base, head)
            })
            .collect();
        // a checkout may have switched branches, and with them the ref to watch
        let vcs_paths = diff_providers.get_watched_paths(&workspace);

        job::dispatch_blocking(move |editor, _| {
            if VCS_RELOAD_GENERATION.load(atomic::Ordering::Relaxed) != generation {
                return;
            }
            for (doc_id, diff_base, head) in updates {
                // the document may have been closed meanwhile
                let Some(doc) = editor.documents.get_mut(&doc_id) else {
                    continue;
                };
                match diff_base {
                    Some(diff_base) => doc.set_diff_base(diff_base),
                    None => doc.diff_handle = None,
                }
                doc.set_version_control_head(head);
            }
            editor.file_watcher.set_vcs_paths(vcs_paths);
        });
    });
}

/// Shows a prompt asking the user whether to reload a modified document.
/// Co-Authored-By: Anthony Rubick <68485672+AnthonyMichaelTDM@users.noreply.github.com>
fn prompt_reload_modified(compositor: &mut Compositor, doc_id: DocumentId, path_str: String) {
    let prompt = Prompt::new(
        Cow::Owned(format!(
            "{path_str} changed externally (unsaved changes exist). Press Enter to reload, Esc to ignore: "
        )),
        None,
        ui::completers::none,
        move |cx, _input, event| {
            match event {
                PromptEvent::Validate => {
                    let scrolloff = cx.editor.config().scrolloff;
                    let target_view_id = cx.editor.get_synced_view_id(doc_id);
                    let trust_full = {
                        let doc = doc!(cx.editor, &doc_id);
                        cx.editor
                            .workspace_trust
                            .query(
                                doc.workspace_root(),
                                helix_loader::workspace_trust::TrustQuery::Git,
                            )
                            .is_trusted()
                    };
                    let doc = doc_mut!(cx.editor, &doc_id);
                    let view = view_mut!(cx.editor, target_view_id);
                    match doc.reload(view, &cx.editor.diff_providers, trust_full) {
                        Ok(_) => {
                            view.ensure_cursor_in_view(doc, scrolloff);
                            cx.editor.set_status(format!("{path_str} reloaded"));
                        }
                        Err(err) => {
                            cx.editor
                                .set_error(format!("{path_str} reload failed: {err}"));
                        }
                    }
                }
                PromptEvent::Abort => {
                    cx.editor
                        .set_status(format!("{path_str} external changes ignored"));
                }
                PromptEvent::Update => {}
            }
        },
    );
    compositor.push(Box::new(prompt));
}

pub(super) fn register_hooks(handlers: &Handlers, config: &Config) {
    // Register handler for FileSystemDidChange events (from filesentry)
    let handler = Arc::new(ReloadHandler {
        enable: config.auto_reload.enable.into(),
        prompt_if_modified: config.auto_reload.prompt_if_modified.into(),
    });
    let handler_ = handler.clone();
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        handler_.refresh_config(event.new);
        // The poll loop reschedules itself and stops when polling is turned off
        // (see `finish_debounce`), and it is only started once below at startup.
        // Restart it when polling is turned back on at runtime, otherwise the
        // setting would stay dead until the editor is restarted.
        let polls = |config: &Config| config.auto_reload.enable && config.auto_reload.poll.enable;
        if polls(event.new) && !polls(event.old) {
            send_blocking(
                &event.editor.handlers.auto_reload,
                AutoReloadEvent::PollAfter {
                    interval: event.new.auto_reload.poll.interval,
                },
            );
        }
        Ok(())
    });
    register_hook!(move |event: &mut FileSystemDidChange| {
        handler.on_file_did_change(event);
        Ok(())
    });

    // Start polling if enabled
    if config.auto_reload.enable && config.auto_reload.poll.enable {
        send_blocking(
            &handlers.auto_reload,
            AutoReloadEvent::PollAfter {
                interval: config.auto_reload.poll.interval,
            },
        );
    }
}
