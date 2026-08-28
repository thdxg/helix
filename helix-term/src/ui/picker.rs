mod handlers;
mod query;

use crate::{
    alt,
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
    ui::{
        self,
        document::{render_document, LinePos, TextRenderer},
        editor,
        picker::query::PickerQuery,
        text_decorations::DecorationManager,
        EditorView,
    },
};
use futures_util::future::BoxFuture;
use helix_event::AsyncHook;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans},
    widgets::{Block, BorderType, Cell, Row, Table},
};

use tui::widgets::Widget;

use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::Path,
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation,
    visual_offset_from_anchor, Position,
};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    input::KeyEvent,
    media::{GraphicsMode, MediaKind, MediaState},
    theme::Style,
    view::ViewPosition,
    Document, DocumentId, Editor,
};

use self::handlers::{
    spawn_preview_raster, DynamicQueryChange, DynamicQueryHandler, PreviewHighlightHandler,
    PreviewMediaHandler,
};

pub const ID: &str = "picker";

/// Which mode a picker's key handling is in, when `editor.picker.modal` is
/// enabled. Modal handling is skipped entirely while it is disabled, so the
/// mode is only ever consulted for a modal picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerMode {
    /// Keys type into the query, as they always do in a non-modal picker.
    #[default]
    Insert,
    /// Unmodified keys drive the picker instead of typing into the query.
    Normal,
}

/// What a key does in [`PickerMode::Normal`]. See [`normal_mode_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormalModeAction {
    MoveNext,
    MovePrevious,
    ToStart,
    ToEnd,
    ScrollPreviewLineDown,
    ScrollPreviewLineUp,
    ScrollPreviewPageDown,
    ScrollPreviewPageUp,
    TogglePreview,
    OpenVerticalSplit,
    OpenHorizontalSplit,
    EnterInsertMode,
    Close,
}

/// The normal-mode keymap of a modal picker.
///
/// `None` means the key has no normal-mode binding of its own and is left to
/// the picker's shared key handling, which still binds `Enter`, `Esc` and the
/// `Ctrl-*` chords in both modes. A key that neither binds is swallowed rather
/// than typed into the query — that is the point of normal mode.
///
/// The key is expected to have been canonicalized (see
/// `ui::editor::canonicalize_key`), so that `Shift-G` and a bare `G` both
/// arrive here as `G`.
fn normal_mode_action(event: KeyEvent) -> Option<NormalModeAction> {
    Some(match event {
        key!('j') => NormalModeAction::MoveNext,
        key!('k') => NormalModeAction::MovePrevious,
        key!('g') => NormalModeAction::ToStart,
        key!('G') => NormalModeAction::ToEnd,
        key!('J') => NormalModeAction::ScrollPreviewLineDown,
        key!('K') => NormalModeAction::ScrollPreviewLineUp,
        key!('f') => NormalModeAction::ScrollPreviewPageDown,
        key!('b') => NormalModeAction::ScrollPreviewPageUp,
        key!('t') => NormalModeAction::TogglePreview,
        key!('v') => NormalModeAction::OpenVerticalSplit,
        key!('s') => NormalModeAction::OpenHorizontalSplit,
        key!('i') | key!('a') | key!('/') => NormalModeAction::EnterInsertMode,
        key!('q') => NormalModeAction::Close,
        _ => return None,
    })
}

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId<'a> {
    Id(DocumentId),
    Path(&'a Path),
}

impl<'a> From<&'a Path> for PathOrId<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl From<DocumentId> for PathOrId<'_> {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>>>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation<'a> = (PathOrId<'a>, Option<(usize, usize)>);

pub enum CachedPreview {
    Document(Box<Document>),
    Directory(Vec<(String, bool)>),
    /// An image or PDF, rendered graphically rather than as text.
    Media(MediaPreview),
    Binary,
    LargeFile,
    NotFound,
}

/// An image or the first page of a PDF. Rasterizing runs off the main thread
/// (it shells out to `magick`/`pdftoppm`), so the preview starts out pending
/// and is filled in by [`handlers::PreviewMediaHandler`].
pub enum MediaPreview {
    /// Not rasterized yet. `started` guards against kicking off a second
    /// rasterize for a page that is already being worked on.
    Rendering {
        kind: MediaKind,
        started: bool,
    },
    Ready(Box<MediaState>),
    /// Rasterizing failed, or the terminal cannot display images: the reason,
    /// shown in place of the preview.
    Unavailable(String),
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    fn dir_content(&self) -> Option<&Vec<(String, bool)>> {
        match self {
            Preview::Cached(CachedPreview::Directory(dir_content)) => Some(dir_content),
            _ => None,
        }
    }

    /// The rasterized image or PDF page to draw, if this preview is one. Media
    /// files already open in the editor keep their state on the document, so
    /// the picker draws whichever page the document is on.
    fn media(&self) -> Option<&MediaState> {
        match self {
            Preview::EditorDocument(doc) => doc.media.as_ref(),
            Preview::Cached(CachedPreview::Media(MediaPreview::Ready(media))) => Some(media),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<Invalid file location>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Directory(_) => "<Invalid directory location>",
                CachedPreview::Media(MediaPreview::Rendering { kind, .. }) => match kind {
                    MediaKind::Pdf => "<Rendering PDF\u{2026}>",
                    MediaKind::Image => "<Rendering image\u{2026}>",
                },
                CachedPreview::Media(MediaPreview::Ready(_)) => "<Invalid file location>",
                CachedPreview::Media(MediaPreview::Unavailable(reason)) => reason,
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

/// Where the [`MediaState`] behind a media preview lives, so that the picker
/// can page it. A media file already open in the editor keeps its state on the
/// document (and so shares the page with the editor's own view of it);
/// otherwise the picker owns it in its preview cache.
enum MediaTarget {
    Document(DocumentId),
    Cached(Arc<Path>),
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    injector.push(item, |item, dst| {
        for (column, text) in columns.iter().filter(|column| column.filter).zip(dst) {
            *text = column.format_text(item, editor_data).into()
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<[Column<T, D>]>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
    /// A marker that requests a redraw when the injector drops.
    /// This marker causes the "running" indicator to disappear when a background job
    /// providing items is finished and drops. This could be wrapped in an [Arc] to ensure
    /// that the redraw is only requested when all Injectors drop for a Picker (which removes
    /// the "running" indicator) but the redraw handle is debounced so this is unnecessary.
    _redraw: helix_event::RequestRedrawOnDrop,
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
            _redraw: helix_event::RequestRedrawOnDrop,
        }
    }
}

#[derive(Error, Debug)]
#[error("picker has been shut down")]
pub struct InjectorShutdown;

impl<T, D> Injector<T, D> {
    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: Arc<str>,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
    hidden: bool,
}

impl<T, D> Column<T, D> {
    pub fn new(name: impl Into<Arc<str>>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            format,
            filter: true,
            hidden: false,
        }
    }

    /// A column which does not display any contents
    pub fn hidden(name: impl Into<Arc<str>>) -> Self {
        let format = |_: &T, _: &D| unreachable!();

        Self {
            name: name.into(),
            format,
            filter: false,
            hidden: true,
        }
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
type DynQueryCallback<T, D> =
    fn(&str, &mut Editor, Arc<D>, &Injector<T, D>) -> BoxFuture<'static, anyhow::Result<()>>;

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: PickerQuery,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,

    callback_fn: PickerCallback<T>,
    default_action: Action,
    /// Extra key bindings, checked before the picker's own key handling.
    /// See [`Picker::with_key_handlers`].
    key_handlers: PickerKeyHandlers<T, D>,
    /// Extra key bindings which are only live in [`PickerMode::Normal`].
    /// See [`Picker::with_modal_key_handlers`].
    modal_key_handlers: PickerKeyHandlers<T, D>,
    /// The picker's modal-editing mode. Only consulted when
    /// `editor.picker.modal` is enabled; a picker always opens in
    /// [`PickerMode::Insert`] so that typing filters right away.
    mode: PickerMode,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<Arc<Path>, CachedPreview>,
    read_buffer: Vec<u8>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,
    /// An event handler for syntax highlighting the currently previewed file.
    preview_highlight_handler: Sender<Arc<Path>>,
    /// An event handler for rasterizing the currently previewed image or PDF.
    preview_media_handler: Sender<Arc<Path>>,
    dynamic_query_handler: Option<Sender<DynamicQueryChange>>,

    /// Vertical scroll of the preview relative to its natural top. Positive
    /// scrolls down, negative up. Counted in visual (soft-wrapped) rows for a
    /// document preview and in placement rows for a panned image; a PDF pages
    /// instead of scrolling, so this stays zero for one.
    preview_scroll_offset: isize,
    /// Height in rows of the preview pane's inner area, used for page scrolling.
    preview_height: u16,
    /// Selected item the current `preview_scroll_offset` applies to; the scroll
    /// resets to the top when the selection changes.
    preview_scroll_cursor: u32,
    /// Whether the last frame actually drew a preview. Preview-scroll keys fall
    /// back to result-list navigation when it did not, so `PageUp`/`PageDown`
    /// keep paging the list in a window too narrow for a preview.
    preview_visible: bool,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(
        columns: impl IntoIterator<Item = Column<T, D>>,
        editor_data: D,
    ) -> (Nucleo<T>, Injector<T, D>) {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns,
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
            _redraw: helix_event::RequestRedrawOnDrop,
        };
        (matcher, streamer)
    }

    pub fn new<C, O, F>(
        columns: C,
        primary_column: usize,
        options: O,
        editor_data: D,
        callback_fn: F,
    ) -> Self
    where
        C: IntoIterator<Item = Column<T, D>>,
        O: IntoIterator<Item = T>,
        F: Fn(&mut Context, &T, Action) + 'static,
    {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns
            .iter()
            .filter(|col: &&Column<T, D>| col.filter)
            .count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        Self::with(
            matcher,
            columns,
            primary_column,
            Arc::new(editor_data),
            Arc::new(AtomicUsize::new(0)),
            callback_fn,
        )
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        Self::with(
            matcher,
            injector.columns,
            primary_column,
            injector.editor_data,
            injector.picker_version,
            callback_fn,
        )
    }

    fn with(
        matcher: Nucleo<T>,
        columns: Arc<[Column<T, D>]>,
        default_column: usize,
        editor_data: Arc<D>,
        version: Arc<AtomicUsize>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let widths = columns
            .iter()
            .map(|column| Constraint::Length(column.name.chars().count() as u16))
            .collect();

        let query = PickerQuery::new(columns.iter().map(|col| &col.name).cloned(), default_column);

        Self {
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query,
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            default_action: Action::Replace,
            key_handlers: HashMap::new(),
            modal_key_handlers: HashMap::new(),
            mode: PickerMode::Insert,
            completion_height: 0,
            widths,
            preview_cache: HashMap::new(),
            read_buffer: Vec::with_capacity(1024),
            file_fn: None,
            preview_highlight_handler: PreviewHighlightHandler::<T, D>::default().spawn(),
            preview_media_handler: PreviewMediaHandler::<T, D>::default().spawn(),
            dynamic_query_handler: None,
            preview_scroll_offset: 0,
            preview_height: 0,
            preview_scroll_cursor: 0,
            preview_visible: false,
        }
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
            _redraw: helix_event::RequestRedrawOnDrop,
        }
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>> + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    pub fn with_history_register(mut self, history_register: Option<char>) -> Self {
        self.prompt.with_history_register(history_register);
        self
    }

    pub fn with_initial_cursor(mut self, cursor: u32) -> Self {
        self.cursor = cursor;
        self
    }

    /// Adds extra key bindings to the picker.
    ///
    /// The handlers are consulted before the picker's own key handling and before
    /// the embedded prompt sees the key, so a handler may shadow a built-in
    /// binding. A handler only runs when the picker has a selection; otherwise the
    /// key falls through to the normal handling.
    ///
    /// Prefer keys that the picker and its prompt do not already use. Notably
    /// taken are `Alt-Enter`, `Alt-b`, `Alt-d`, `Alt-f`, `Alt-Backspace`,
    /// `Alt-Delete`, `Ctrl-u`, `Ctrl-d`, `PageUp` and `PageDown`.
    ///
    /// ```ignore
    /// let picker = Picker::new(columns, 0, options, data, callback)
    ///     .with_key_handlers(hashmap! {
    ///         alt!('y') => Box::new(|cx, args: PickerAction<'_, MyItem, MyData>| {
    ///             cx.editor.set_status(args.selection.to_string());
    ///         }) as PickerKeyHandler<MyItem, MyData>,
    ///     });
    /// ```
    pub fn with_key_handlers(mut self, handlers: PickerKeyHandlers<T, D>) -> Self {
        self.key_handlers = handlers;
        self
    }

    /// Adds key bindings which are only live in [`PickerMode::Normal`], that is
    /// only when `editor.picker.modal` is enabled and the user has left the
    /// query with `Esc`.
    ///
    /// Because these keys never reach the query, they can be unmodified letters
    /// which would otherwise be typed into it. Bind the same handlers here as in
    /// [`Picker::with_key_handlers`] to offer a bare-key alternative to a
    /// modifier chord.
    ///
    /// ```ignore
    /// let picker = Picker::new(columns, 0, options, data, callback)
    ///     .with_key_handlers(hashmap! {
    ///         alt!('y') => file_operation_key(yank_selected_path),
    ///     })
    ///     .with_modal_key_handlers(hashmap! {
    ///         key!('y') => file_operation_key(yank_selected_path),
    ///     });
    /// ```
    pub fn with_modal_key_handlers(mut self, handlers: PickerKeyHandlers<T, D>) -> Self {
        self.modal_key_handlers = handlers;
        self
    }

    /// Runs the [`Picker::with_key_handlers`] handler bound to `event`, if any.
    ///
    /// Returns `true` when a handler ran and the key should not be handled any
    /// further.
    fn handle_custom_key(&self, event: &KeyEvent, cx: &mut Context) -> bool {
        self.run_key_handler(&self.key_handlers, event, cx)
    }

    /// Runs the [`Picker::with_modal_key_handlers`] handler bound to `event`, if
    /// any. Only called in [`PickerMode::Normal`].
    fn handle_modal_key(&self, event: &KeyEvent, cx: &mut Context) -> bool {
        self.run_key_handler(&self.modal_key_handlers, event, cx)
    }

    fn run_key_handler(
        &self,
        handlers: &PickerKeyHandlers<T, D>,
        event: &KeyEvent,
        cx: &mut Context,
    ) -> bool {
        let Some(handler) = handlers.get(event) else {
            return false;
        };
        let Some(selection) = self.selection() else {
            return false;
        };

        handler(
            cx,
            PickerAction {
                selection,
                data: Arc::clone(&self.editor_data),
                cursor: self.cursor,
            },
        );

        true
    }

    /// Whether the picker is currently in modal normal mode, where unmodified
    /// keys drive the picker rather than typing into the query.
    ///
    /// Always `false` while `editor.picker.modal` is disabled, which is the
    /// default: turning the option off restores exactly the pre-modal
    /// behaviour, `Esc` closing the picker included.
    fn in_normal_mode(&self, editor: &Editor) -> bool {
        editor.config().picker.modal && self.mode == PickerMode::Normal
    }

    pub fn with_dynamic_query(
        mut self,
        callback: DynQueryCallback<T, D>,
        debounce_ms: Option<u64>,
    ) -> Self {
        let handler = DynamicQueryHandler::new(callback, debounce_ms).spawn();
        let event = DynamicQueryChange {
            query: self.primary_query(),
            // Treat the initial query as a paste.
            is_paste: true,
        };
        helix_event::send_blocking(&handler, event);
        self.dynamic_query_handler = Some(handler);
        self
    }

    pub fn with_default_action(mut self, action: Action) -> Self {
        self.default_action = action;
        self
    }

    /// Whether a scrollable preview is currently shown. Pickers without a
    /// preview callback (no `file_fn`), with the preview toggled off, or too
    /// narrow to fit one route preview-scroll keys back to result-list
    /// navigation.
    fn preview_shown(&self) -> bool {
        self.show_preview && self.file_fn.is_some() && self.preview_visible
    }

    /// Scroll the preview by `amount` rows, down for a positive amount and up
    /// for a negative one.
    ///
    /// A document preview scrolls by visual (soft-wrapped) rows, so a file with
    /// long lines can be read to its end one wrapped row at a time; the offset
    /// is clamped against the file bounds later, when the preview is rendered
    /// and the soft-wrap layout is known. An image pans by placement rows,
    /// clamped against the placement when it is drawn. A PDF pages instead:
    /// every scroll key turns exactly one page, like `gj`/`gk` and the mouse
    /// wheel do in a media view.
    fn scroll_preview(&mut self, amount: isize, editor: &mut Editor) {
        match self.selected_media(editor) {
            Some((target, MediaKind::Pdf)) => self.page_media(editor, &target, amount.signum()),
            // Clamped at the top here and at the bottom of the placement when
            // the image is drawn, so overscroll can't accumulate in either
            // direction.
            Some((_, MediaKind::Image)) => {
                self.preview_scroll_offset =
                    self.preview_scroll_offset.saturating_add(amount).max(0)
            }
            None => self.preview_scroll_offset = self.preview_scroll_offset.saturating_add(amount),
        }
    }

    /// Scroll the preview down one line (`scroll-lines` rows of it).
    pub fn scroll_preview_line_down(&mut self, editor: &mut Editor) {
        let lines = editor.config().scroll_lines.unsigned_abs() as isize;
        self.scroll_preview(lines, editor);
    }

    /// Scroll the preview up one line (`scroll-lines` rows of it).
    pub fn scroll_preview_line_up(&mut self, editor: &mut Editor) {
        let lines = editor.config().scroll_lines.unsigned_abs() as isize;
        self.scroll_preview(-lines, editor);
    }

    /// Scroll the preview down by the height of the preview pane.
    pub fn scroll_preview_page_down(&mut self, editor: &mut Editor) {
        self.scroll_preview(self.preview_height as isize, editor);
    }

    /// Scroll the preview up by the height of the preview pane.
    pub fn scroll_preview_page_up(&mut self, editor: &mut Editor) {
        self.scroll_preview(-(self.preview_height as isize), editor);
    }

    /// Where the media state of the currently selected preview lives, and what
    /// kind of media it is. `None` when the selection has no preview or its
    /// preview is not a rasterized image or PDF (including one that has not
    /// finished rasterizing yet, which has no page to turn).
    fn selected_media(&self, editor: &Editor) -> Option<(MediaTarget, MediaKind)> {
        let current = self.selection()?;
        let (path_or_id, _) = (self.file_fn.as_ref()?)(editor, current)?;

        let id = match path_or_id {
            PathOrId::Id(id) => id,
            PathOrId::Path(path) => match editor.document_by_path(path) {
                Some(doc) => doc.id(),
                None => {
                    // NOTE: `get_key_value` rather than indexing, to get an
                    // owned key that outlives the borrow of `path`.
                    let (path, preview) = self.preview_cache.get_key_value(path)?;
                    let CachedPreview::Media(MediaPreview::Ready(media)) = preview else {
                        return None;
                    };
                    return Some((MediaTarget::Cached(path.clone()), media.kind));
                }
            },
        };
        let kind = editor.documents.get(&id)?.media.as_ref()?.kind;
        Some((MediaTarget::Document(id), kind))
    }

    fn media_state_mut<'a>(
        preview_cache: &'a mut HashMap<Arc<Path>, CachedPreview>,
        editor: &'a mut Editor,
        target: &MediaTarget,
    ) -> Option<&'a mut MediaState> {
        match target {
            MediaTarget::Document(id) => editor.documents.get_mut(id)?.media.as_mut(),
            MediaTarget::Cached(path) => match preview_cache.get_mut(path)? {
                CachedPreview::Media(MediaPreview::Ready(media)) => Some(media),
                _ => None,
            },
        }
    }

    /// Turn a PDF preview by `pages`, clamped to the document. Paging only
    /// moves a counter; the page it lands on is rasterized off the main thread
    /// by `request_media_raster` on the next frame, so holding the key neither
    /// blocks the picker nor renders pages that go by on the way.
    fn page_media(&mut self, editor: &mut Editor, target: &MediaTarget, pages: isize) {
        let Some(media) = Self::media_state_mut(&mut self.preview_cache, editor, target) else {
            return;
        };
        let mut page = (media.page as isize).saturating_add(pages).max(0) as usize;
        if let Some(count) = media.page_count {
            page = page.min(count.saturating_sub(1));
        }
        if page == media.page {
            // Stay quiet at either end of the document.
            return;
        }
        if let Err(err) = media.goto_page(page) {
            // Unknown page count (no `pdfinfo`): paging past the end fails.
            editor.set_error(err.to_string());
        }
    }

    /// Kick off the rasterize for a PDF preview that has been paged, if one is
    /// outstanding and none is already running. Called every frame, as the
    /// editor's media view does, so the page landed on is picked up even when
    /// several pages went by while a rasterize was in flight.
    fn request_media_raster(&mut self, editor: &mut Editor) {
        if editor.graphics.mode == GraphicsMode::None {
            return;
        }
        let Some((target, MediaKind::Pdf)) = self.selected_media(editor) else {
            return;
        };
        let Some(media) = Self::media_state_mut(&mut self.preview_cache, editor, &target) else {
            return;
        };
        let Some(request) = media.take_raster_request() else {
            return;
        };
        match target {
            MediaTarget::Document(id) => EditorView::spawn_raster(id, request),
            MediaTarget::Cached(path) => spawn_preview_raster::<T, D>(path, request),
        }
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let len = self.matcher.snapshot().matched_item_count();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self
            .matcher
            .snapshot()
            .matched_item_count()
            .saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    fn primary_query(&self) -> Arc<str> {
        self.query
            .get(&self.columns[self.primary_column].name)
            .cloned()
            .unwrap_or_else(|| "".into())
    }

    fn header_height(&self) -> u16 {
        if self.columns.len() > 1 {
            1
        } else {
            0
        }
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            self.handle_prompt_change(matches!(event, Event::Paste(_)));
        }
        EventResult::Consumed(None)
    }

    fn handle_prompt_change(&mut self, is_paste: bool) {
        // TODO: better track how the pattern has changed
        let line = self.prompt.line();
        let old_query = self.query.parse(line);
        if self.query == old_query {
            return;
        }
        // If the query has meaningfully changed, reset the cursor to the top of the results.
        self.cursor = 0;
        // Have nucleo reparse each changed column.
        for (i, column) in self
            .columns
            .iter()
            .filter(|column| column.filter)
            .enumerate()
        {
            let pattern = self
                .query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            let old_pattern = old_query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            // Fastlane: most columns will remain unchanged after each edit.
            if pattern == old_pattern {
                continue;
            }
            let is_append = pattern.starts_with(old_pattern);
            self.matcher.pattern.reparse(
                i,
                pattern,
                CaseMatching::Smart,
                Normalization::Smart,
                is_append,
            );
        }
        // If this is a dynamic picker, notify the query hook that the primary
        // query might have been updated.
        if let Some(handler) = &self.dynamic_query_handler {
            let event = DynamicQueryChange {
                query: self.primary_query(),
                is_paste,
            };
            helix_event::send_blocking(handler, event);
        }
    }

    /// Get (cached) preview for the currently selected item. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        editor: &'editor Editor,
    ) -> Option<(Preview<'picker, 'editor>, Option<(usize, usize)>)> {
        let current = self.selection()?;
        let (path_or_id, range) = (self.file_fn.as_ref()?)(editor, current)?;

        match path_or_id {
            PathOrId::Path(path) => {
                if let Some(doc) = editor.document_by_path(path) {
                    return Some((Preview::EditorDocument(doc), range));
                }

                if self.preview_cache.contains_key(path) {
                    // NOTE: we use `HashMap::get_key_value` here instead of indexing so we can
                    // retrieve the `Arc<Path>` key. The `path` in scope here is a `&Path` and
                    // we can cheaply clone the key for the preview highlight handler.
                    let (path, preview) = self.preview_cache.get_key_value(path).unwrap();
                    if matches!(preview, CachedPreview::Document(doc) if doc.syntax().is_none()) {
                        helix_event::send_blocking(&self.preview_highlight_handler, path.clone());
                    }
                    if matches!(
                        preview,
                        CachedPreview::Media(MediaPreview::Rendering { started: false, .. })
                    ) {
                        helix_event::send_blocking(&self.preview_media_handler, path.clone());
                    }
                    return Some((Preview::Cached(preview), range));
                }

                let path: Arc<Path> = path.into();
                let preview = std::fs::metadata(&path)
                    .and_then(|metadata| {
                        if metadata.is_dir() {
                            let files = super::directory_content(&path, editor)?;
                            let file_names: Vec<_> = files
                                .iter()
                                .filter_map(|(file_path, is_dir)| {
                                    let name = file_path
                                        .strip_prefix(&path)
                                        .map(|p| Some(p.as_os_str()))
                                        .unwrap_or_else(|_| file_path.file_name())?
                                        .to_string_lossy();
                                    if *is_dir {
                                        Some((format!("{}/", name), true))
                                    } else {
                                        Some((name.into_owned(), false))
                                    }
                                })
                                .collect();
                            Ok(CachedPreview::Directory(file_names))
                        } else if metadata.is_file() {
                            // Images and PDFs are rendered graphically instead
                            // of being read as text, so the size cap (which is
                            // about loading a file into a rope) does not apply
                            // to them.
                            if let Some(kind) = helix_view::media::detect_kind(&path) {
                                return Ok(CachedPreview::Media(
                                    if editor.graphics.mode == GraphicsMode::None {
                                        MediaPreview::Unavailable(
                                            "<No image support in this terminal>".to_string(),
                                        )
                                    } else {
                                        MediaPreview::Rendering {
                                            kind,
                                            started: false,
                                        }
                                    },
                                ));
                            }
                            if metadata.len() > MAX_FILE_SIZE_FOR_PREVIEW {
                                return Ok(CachedPreview::LargeFile);
                            }
                            let is_binary = std::fs::File::open(&path).and_then(|file| {
                                // Read up to 1kb to detect the content type
                                let n = file.take(1024).read_to_end(&mut self.read_buffer)?;
                                let is_binary = crate::is_binary(&self.read_buffer[..n]);
                                self.read_buffer.clear();
                                Ok(is_binary)
                            })?;
                            if is_binary {
                                return Ok(CachedPreview::Binary);
                            }
                            let mut doc = Document::open(
                                &path,
                                None,
                                false,
                                editor.config.clone(),
                                editor.syn_loader.clone(),
                            )
                            .or(Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Cannot open document",
                            )))?;
                            let loader = editor.syn_loader.load();
                            if let Some(language_config) = doc.detect_language_config(&loader) {
                                doc.language = Some(language_config);
                                // Asynchronously highlight the new document
                                helix_event::send_blocking(
                                    &self.preview_highlight_handler,
                                    path.clone(),
                                );
                            }
                            Ok(CachedPreview::Document(Box::new(doc)))
                        } else {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Neither a dir, nor a file",
                            ))
                        }
                    })
                    .unwrap_or(CachedPreview::NotFound);
                if matches!(
                    preview,
                    CachedPreview::Media(MediaPreview::Rendering { started: false, .. })
                ) {
                    helix_event::send_blocking(&self.preview_media_handler, path.clone());
                }
                self.preview_cache.insert(path.clone(), preview);
                Some((Preview::Cached(&self.preview_cache[&path]), range))
            }
            PathOrId::Id(id) => {
                let doc = editor.documents.get(&id).unwrap();
                Some((Preview::EditorDocument(doc), range))
            }
        }
    }

    fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.matcher.tick(10);
        let snapshot = self.matcher.snapshot();
        if status.changed {
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlight_style = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);

        BLOCK.render(area, surface);

        // -- Render the input bar:

        let count = format!(
            "{}{}/{}",
            if status.running || self.matcher.active_injectors() > 0 {
                "(running) "
            } else {
                ""
            },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );

        // The mode indicator only exists for a modal picker; a non-modal one
        // renders its prompt line exactly as it always has.
        let mode = cx.editor.config().picker.modal.then(|| match self.mode {
            PickerMode::Insert => ("INS ", cx.editor.theme.get("ui.statusline.insert")),
            PickerMode::Normal => ("NOR ", cx.editor.theme.get("ui.statusline.normal")),
        });
        let mode_width = mode.map_or(0, |(label, _)| label.len() as u16);

        let area = inner.clip_left(1).with_height(1);
        let line_area = area
            .clip_left(mode_width)
            .clip_right(count.len() as u16 + 1);

        if let Some((label, style)) = mode {
            surface.set_stringn(
                area.x,
                area.y,
                label,
                (mode_width as usize).min(area.width as usize),
                style,
            );
        }

        // render the prompt first since it will clear its background
        self.prompt.render(line_area, surface, cx);

        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of prompt from top
        let inner = inner.clip_top(2);
        let rows = inner.height.saturating_sub(self.header_height()) as u32;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows));
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() {
            matcher.config.set_match_paths()
        }

        let options = snapshot.matched_items(offset..end).map(|item| {
            let mut widths = self.widths.iter_mut();
            let mut matcher_index = 0;

            Row::new(self.columns.iter().map(|column| {
                if column.hidden {
                    return Cell::default();
                }

                let Some(Constraint::Length(max_width)) = widths.next() else {
                    unreachable!();
                };
                let mut cell = column.format(item.data, &self.editor_data);
                let width = if column.filter {
                    snapshot.pattern().column_pattern(matcher_index).indices(
                        item.matcher_columns[matcher_index].slice(..),
                        &mut matcher,
                        &mut indices,
                    );
                    indices.sort_unstable();
                    indices.dedup();
                    let mut indices = indices.drain(..);
                    let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                    let mut span_list = Vec::new();
                    let mut current_span = String::new();
                    let mut current_style = Style::default();
                    let mut grapheme_idx = 0u32;
                    let mut width = 0;

                    let spans: &[Span] =
                        cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                    for span in spans {
                        // this looks like a bug on first glance, we are iterating
                        // graphemes but treating them as char indices. The reason that
                        // this is correct is that nucleo will only ever consider the first char
                        // of a grapheme (and discard the rest of the grapheme) so the indices
                        // returned by nucleo are essentially grapheme indecies
                        for grapheme in span.content.graphemes(true) {
                            let style = if grapheme_idx == next_highlight_idx {
                                next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                span.style.patch(highlight_style)
                            } else {
                                span.style
                            };
                            if style != current_style {
                                if !current_span.is_empty() {
                                    span_list.push(Span::styled(current_span, current_style))
                                }
                                current_span = String::new();
                                current_style = style;
                            }
                            current_span.push_str(grapheme);
                            grapheme_idx += 1;
                        }
                        width += span.width();
                    }

                    span_list.push(Span::styled(current_span, current_style));
                    cell = Cell::from(Spans::from(span_list));
                    matcher_index += 1;
                    width
                } else {
                    cell.content
                        .lines
                        .first()
                        .map(|line| line.width())
                        .unwrap_or_default()
                };

                if width as u16 > *max_width {
                    *max_width = width as u16;
                }

                cell
            }))
        });

        let mut table = Table::new(options)
            .style(text_style)
            .highlight_style(selected)
            .highlight_symbol(" > ")
            .column_spacing(1)
            .widths(&self.widths);

        // -- Header
        if self.columns.len() > 1 {
            let active_column = self.query.active_column(self.prompt.position());
            let header_style = cx.editor.theme.get("ui.picker.header");
            let header_column_style = cx.editor.theme.get("ui.picker.header.column");

            table = table.header(
                Row::new(self.columns.iter().map(|column| {
                    if column.hidden {
                        Cell::default()
                    } else {
                        let style =
                            if active_column.is_some_and(|name| Arc::ptr_eq(name, &column.name)) {
                                cx.editor.theme.get("ui.picker.header.column.active")
                            } else {
                                header_column_style
                            };

                        Cell::from(Span::styled(Cow::from(&*column.name), style))
                    }
                }))
                .style(header_style),
            );
        }

        use tui::widgets::TableState;

        table.render_table(
            inner,
            surface,
            &mut TableState {
                offset: 0,
                selected: Some(cursor as usize),
            },
            self.truncate_start,
        );
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        let directory = cx.editor.theme.get("ui.text.directory");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(margin);
        BLOCK.render(area, surface);

        // Reset the preview scroll on a selection change, before the
        // `get_preview` borrow below.
        if self.cursor != self.preview_scroll_cursor {
            self.preview_scroll_offset = 0;
            self.preview_scroll_cursor = self.cursor;
        }

        // Pick up the rasterize for a PDF preview that has been paged. Must
        // happen before `get_preview` borrows the editor.
        self.request_media_raster(cx.editor);

        // `get_preview` borrows `self` for the block below, so the offset is
        // read into a local here, clamped against the layout inside, then
        // written back after.
        let mut scroll = self.preview_scroll_offset;

        if let Some((preview, range)) = self.get_preview(cx.editor) {
            // Images and PDFs are drawn as a graphics placement rather than as
            // text. Everything needed is copied out of the preview so that the
            // borrow of the editor ends before its graphics state is touched.
            let media = preview.media().map(|media| {
                (
                    media.raster.clone(),
                    media.kind,
                    media.page,
                    media.page_count,
                    media.is_rastering(),
                )
            });
            if let Some((raster, kind, page, page_count, rastering)) = media {
                let caption = match kind {
                    MediaKind::Pdf => {
                        let mut caption = match page_count {
                            Some(count) => format!("page {}/{}", page + 1, count),
                            None => format!("page {}", page + 1),
                        };
                        // The image below the caption is still the previous page.
                        if rastering {
                            caption.push_str(" \u{2026}");
                        }
                        caption
                    }
                    MediaKind::Image => format!("{}\u{00d7}{}", raster.width, raster.height),
                };
                // The last line is left for the caption.
                let image_area = inner.clip_bottom(1);
                // Only images pan; a PDF turns pages instead and is always
                // drawn whole.
                let pan = if kind == MediaKind::Image {
                    scroll.clamp(0, u16::MAX as isize) as u16
                } else {
                    0
                };
                let placement = ui::media::draw_raster_panned(
                    surface,
                    &mut cx.editor.graphics,
                    image_area,
                    &raster,
                    kind == MediaKind::Pdf,
                    pan,
                );
                let comment = cx.editor.theme.get("comment");
                if let Some(placement) = placement {
                    // Clamp the stored offset against the placement, so
                    // panning can't run off the bottom of the image and
                    // accumulate: the next scroll up then responds at once.
                    self.preview_scroll_offset = pan.min(placement.max_pan) as isize;
                    let mut caption = caption;
                    if placement.max_pan > 0 {
                        // Where the visible window sits in the panned image.
                        caption.push_str(&format!(
                            " \u{2014} row {}/{}",
                            pan.min(placement.max_pan) + 1,
                            placement.max_pan + placement.area.height
                        ));
                    }
                    let x =
                        inner.x + inner.width.saturating_sub(ui::media::text_width(&caption)) / 2;
                    let y = placement.area.y + placement.area.height;
                    surface.set_stringn(x, y, &caption, inner.width as usize, comment);
                } else if cx.editor.graphics.mode == GraphicsMode::None {
                    // A media document open in the editor can reach here with
                    // graphics turned off; cached previews are marked
                    // unavailable up front instead.
                    let alt_text = "<No image support in this terminal>";
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                }
                return;
            }

            let doc = match preview.document() {
                Some(doc)
                    if range.is_none_or(|(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    if let Some(dir_content) = preview.dir_content() {
                        for (i, (path, is_dir)) in
                            dir_content.iter().take(inner.height as usize).enumerate()
                        {
                            let style = if *is_dir { directory } else { text };
                            surface.set_stringn(
                                inner.x,
                                inner.y + i as u16,
                                path,
                                inner.width as usize,
                                style,
                            );
                        }
                        return;
                    }

                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };
            let doc_height = doc.text().len_lines();

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            // Apply the preview scroll by moving the anchor by whole visual
            // lines, so soft-wrapped lines scroll one wrapped row at a time.
            // `offset.anchor` is the natural top of the preview (line 0, or the
            // centred match for range previews) and is left untouched when
            // there is no scroll, keeping that centring. An upward scroll is
            // clamped to the start of the file by `char_idx_at_visual_offset`.
            let mut at_bottom = false;
            if scroll != 0 {
                let text = doc.text().slice(..);
                let text_fmt = doc.text_format(inner.width, None);
                let annotations = TextAnnotations::default();
                let natural_anchor = offset.anchor;

                let (anchor, vertical_offset) = char_idx_at_visual_offset(
                    text,
                    natural_anchor,
                    scroll,
                    0,
                    &text_fmt,
                    &annotations,
                );

                // The anchor that keeps the file's last visual line on the
                // bottom row, so a downward scroll can't run past the end into
                // empty space. Found by walking up one viewport from the end: a
                // screenful of rows at most.
                let (max_anchor, _) = char_idx_at_visual_offset(
                    text,
                    text.len_chars().saturating_sub(1),
                    -(inner.height as isize - 1),
                    0,
                    &text_fmt,
                    &annotations,
                );

                if anchor >= max_anchor {
                    at_bottom = true;
                    offset.anchor = max_anchor;
                    offset.vertical_offset = 0;

                    // Scrolled past the bottom: clamp the stored offset to the
                    // scroll that exactly reaches it, so the offset can't
                    // accumulate and the next upward scroll responds at once.
                    // Measured between the in-range natural and bottom anchors
                    // (not the applied anchor, which may sit past EOF where it
                    // can't be measured), capped at the current offset.
                    if anchor > max_anchor {
                        let max_scroll = visual_offset_from_anchor(
                            text,
                            natural_anchor,
                            max_anchor,
                            &text_fmt,
                            &annotations,
                            scroll.unsigned_abs(),
                        )
                        .map_or(scroll, |(pos, _)| pos.row as isize);
                        scroll = scroll.min(max_scroll);
                    }
                } else {
                    offset.anchor = anchor;
                    offset.vertical_offset = vertical_offset;

                    // Past the top: `char_idx_at_visual_offset` already pinned
                    // the anchor to the start, so normalise a negative offset
                    // to the rows actually scrolled, keeping it from
                    // accumulating above the top.
                    if scroll < 0 && anchor <= natural_anchor {
                        scroll = visual_offset_from_anchor(
                            text,
                            anchor,
                            natural_anchor,
                            &text_fmt,
                            &annotations,
                            scroll.unsigned_abs(),
                        )
                        .map_or(scroll, |(pos, _)| -(pos.row as isize));
                    }
                }
            }

            let loader = cx.editor.syn_loader.load();
            let config = cx.editor.config();

            let syntax_highlighter =
                EditorView::doc_syntax_highlighter(doc, offset.anchor, area.height, &loader);
            let mut overlay_highlights = Vec::new();
            if doc
                .language_config()
                .and_then(|config| config.rainbow_brackets)
                .unwrap_or(config.rainbow_brackets)
            {
                if let Some(overlay) = EditorView::doc_rainbow_highlights(
                    doc,
                    offset.anchor,
                    area.height,
                    &cx.editor.theme,
                    &loader,
                ) {
                    overlay_highlights.push(overlay);
                }
            }

            EditorView::doc_diagnostics_highlights_into(
                doc,
                &cx.editor.theme,
                &mut overlay_highlights,
            );

            let mut decorations = DecorationManager::default();

            if let Some((start, end)) = range {
                let style = cx
                    .editor
                    .theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| cx.editor.theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.set_style(area, style)
                    }
                };
                decorations.add_decoration(draw_highlight);
            }

            let current_line = doc.text().slice(..).char_to_line(offset.anchor);

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                syntax_highlighter,
                overlay_highlights,
                &cx.editor.theme,
                decorations,
            );

            // Scroll indicator on the right edge. The thumb is placed from
            // document lines, which is approximate when lines soft-wrap, so it
            // is pinned to the end once the preview is scrolled to the bottom
            // and otherwise clamped within the track.
            let win_height = inner.height as usize;
            let scroll_style = cx.editor.theme.get("ui.menu.scroll");

            if doc_height > win_height {
                let scroll_height = win_height.pow(2).div_ceil(doc_height).min(win_height);
                let track = win_height - scroll_height;
                let scroll_line = if at_bottom {
                    track
                } else {
                    (track * current_line / std::cmp::max(1, doc_height.saturating_sub(win_height)))
                        .min(track)
                };

                let mut cell;
                for i in 0..win_height {
                    cell = &mut surface[(inner.right() - 1, inner.top() + i as u16)];
                    cell.set_symbol("▐");

                    if scroll_line <= i && i < scroll_line + scroll_height {
                        // thumb
                        cell.set_fg(scroll_style.fg.unwrap_or(helix_view::theme::Color::Reset));
                    } else {
                        // track
                        cell.set_fg(scroll_style.bg.unwrap_or(helix_view::theme::Color::Reset));
                    }
                }
            }
        }

        // Persist the offset clamped against the layout above.
        self.preview_scroll_offset = scroll;
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;
        // Remembered for `preview_shown`, so the preview-scroll keys know
        // whether there is a preview on screen to scroll.
        self.preview_visible = render_preview;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.render_preview(preview_area, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        let key_event = match event {
            Event::Key(event) => *event,
            // A paste is query text, so it is swallowed in normal mode just
            // like a typed key would be.
            Event::Paste(..) if self.in_normal_mode(ctx.editor) => {
                return EventResult::Consumed(None)
            }
            Event::Paste(..) => return self.prompt_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            // Picker is a modal and should consume mouse events so clicks don't fall
            // through to the editor underneath
            Event::Mouse(_) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = |picker: &mut Self| {
            let id = picker.id().unwrap_or_else(|| picker.type_name());

            // if the picker is very large don't store it as last_picker to avoid
            // excessive memory consumption
            let callback: compositor::Callback =
                if picker.matcher.snapshot().item_count() > 1_000_000 {
                    Box::new(|compositor: &mut Compositor, _ctx| {
                        // remove the layer
                        compositor.remove(id);
                    })
                } else {
                    // stop streaming in new items in the background, really we should
                    // be restarting the stream somehow once the picker gets
                    // reopened instead (like for an FS crawl) that would also remove the
                    // need for the special case above but that is pretty tricky
                    picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                    Box::new(|compositor: &mut Compositor, _ctx| {
                        // remove the layer
                        compositor.last_picker = compositor.remove(id);
                    })
                };
            EventResult::Consumed(Some(callback))
        };

        // Extra key bindings supplied by the picker's creator take precedence over
        // the built-in ones.
        if self.handle_custom_key(&key_event, ctx) {
            return EventResult::Consumed(None);
        }

        // Modal editing, when `editor.picker.modal` is enabled. `Esc` leaves the
        // query for normal mode instead of closing the picker, and normal mode
        // binds unmodified keys to the picker's actions. Everything below this
        // block — `Enter`, the `Ctrl-*` bindings, the arrow keys — keeps working
        // identically in both modes, and none of it is reached at all while the
        // option is off.
        if ctx.editor.config().picker.modal {
            // A terminal speaking the Kitty keyboard protocol reports `G` as
            // `Shift-G`; fold that back so both spellings hit the same binding.
            let mut key_event = key_event;
            editor::canonicalize_key(&mut key_event);

            match self.mode {
                PickerMode::Insert => {
                    if key_event == key!(Esc) {
                        self.mode = PickerMode::Normal;
                        return EventResult::Consumed(None);
                    }
                }
                PickerMode::Normal => {
                    // Bare-key bindings supplied by the picker's creator, which
                    // exist only in normal mode.
                    if self.handle_modal_key(&key_event, ctx) {
                        return EventResult::Consumed(None);
                    }

                    // The preview-scroll keys are guarded like the `Alt-j` and
                    // `Alt-k` arms below: with no preview on screen there is
                    // nothing to scroll, so they act as if unbound.
                    let action = match normal_mode_action(key_event) {
                        Some(
                            NormalModeAction::ScrollPreviewLineDown
                            | NormalModeAction::ScrollPreviewLineUp
                            | NormalModeAction::ScrollPreviewPageDown
                            | NormalModeAction::ScrollPreviewPageUp,
                        ) if !self.preview_shown() => None,
                        action => action,
                    };

                    // An unbound key falls through to the shared handling
                    // below. `Esc` closes there, as do `Enter` and the `Ctrl-*`
                    // bindings; anything the shared handling does not bind
                    // either reaches its fallback arm, which swallows the key in
                    // normal mode rather than typing it into the query.
                    if let Some(action) = action {
                        match action {
                            NormalModeAction::Close => return close_fn(self),
                            NormalModeAction::OpenVerticalSplit => {
                                if let Some(option) = self.selection() {
                                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                                }
                                return close_fn(self);
                            }
                            NormalModeAction::OpenHorizontalSplit => {
                                if let Some(option) = self.selection() {
                                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                                }
                                return close_fn(self);
                            }
                            NormalModeAction::MoveNext => self.move_by(1, Direction::Forward),
                            NormalModeAction::MovePrevious => self.move_by(1, Direction::Backward),
                            NormalModeAction::ToStart => self.to_start(),
                            NormalModeAction::ToEnd => self.to_end(),
                            NormalModeAction::ScrollPreviewLineDown => {
                                self.scroll_preview_line_down(ctx.editor)
                            }
                            NormalModeAction::ScrollPreviewLineUp => {
                                self.scroll_preview_line_up(ctx.editor)
                            }
                            NormalModeAction::ScrollPreviewPageDown => {
                                self.scroll_preview_page_down(ctx.editor)
                            }
                            NormalModeAction::ScrollPreviewPageUp => {
                                self.scroll_preview_page_up(ctx.editor)
                            }
                            NormalModeAction::TogglePreview => self.toggle_preview(),
                            NormalModeAction::EnterInsertMode => self.mode = PickerMode::Insert,
                        }

                        return EventResult::Consumed(None);
                    }
                }
            }
        }

        match key_event {
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            // Ctrl-d/Ctrl-u always page the result list. PageDown/PageUp
            // scroll the preview a full page when one is shown, and otherwise
            // page the list.
            ctrl!('d') => {
                self.page_down();
            }
            ctrl!('u') => {
                self.page_up();
            }
            key!(PageDown) if self.preview_shown() => {
                self.scroll_preview_page_down(ctx.editor);
            }
            key!(PageUp) if self.preview_shown() => {
                self.scroll_preview_page_up(ctx.editor);
            }
            key!(PageDown) => {
                self.page_down();
            }
            key!(PageUp) => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => return close_fn(self),
            alt!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, self.default_action);
                }
            }
            key!(Enter) => {
                // If the prompt has a history completion and is empty, use enter to accept
                // that completion
                if let Some(completion) = self
                    .prompt
                    .first_history_completion(ctx.editor)
                    .filter(|_| self.prompt.line().is_empty())
                {
                    // The percent character is used by the query language and needs to be
                    // escaped with a backslash.
                    let completion = if completion.contains('%') {
                        completion.replace('%', "\\%")
                    } else {
                        completion.into_owned()
                    };
                    self.prompt.set_line(completion, ctx.editor);

                    // Inserting from the history register is a paste.
                    self.handle_prompt_change(true);
                } else {
                    if let Some(option) = self.selection() {
                        (self.callback_fn)(ctx, option, self.default_action);
                    }
                    if let Some(history_register) = self.prompt.history_register() {
                        if let Err(err) = ctx
                            .editor
                            .registers
                            .push(history_register, self.primary_query().to_string())
                        {
                            ctx.editor.set_error(err.to_string());
                        }
                    }
                    return close_fn(self);
                }
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return close_fn(self);
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return close_fn(self);
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            // Preview line scrolling. Alt-d/f/b are intentionally avoided here:
            // the prompt keybinds (which also apply in pickers) use them for
            // word editing in the query, and full-page preview scrolling is
            // already on PageUp/PageDown.
            alt!('k') | shift!(Up) if self.preview_shown() => {
                self.scroll_preview_line_up(ctx.editor);
            }
            alt!('j') | shift!(Down) if self.preview_shown() => {
                self.scroll_preview_line_down(ctx.editor);
            }
            // In modal normal mode an unbound key is swallowed. Handing it to
            // the prompt would type it into the query, which is exactly what
            // normal mode exists to avoid.
            _ if self.in_normal_mode(ctx.editor) => {}
            _ => {
                self.prompt_handle_event(event, ctx);
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::bordered();
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };
        // Keep in step with `render_picker`: a modal picker reserves room for
        // the mode indicator at the head of the prompt line.
        let mode_width = if editor.config().picker.modal { 4 } else { 0 };
        let area = inner
            .clip_left(1)
            .with_height(1)
            .with_width(picker_width)
            .clip_left(mode_width);

        self.prompt.cursor(area, editor)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = height.saturating_sub(4 + self.header_height());
        // The preview pane's inner height: the box borders take two rows.
        self.preview_height = height.saturating_sub(2);
        Some((width, height))
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action)>;

/// The picker state handed to a [`PickerKeyHandler`] when its key is pressed.
pub struct PickerAction<'a, T, D> {
    /// The item currently under the picker's cursor.
    pub selection: &'a T,
    /// The data shared by every item of the picker, as passed to
    /// [`Picker::new`].
    pub data: Arc<D>,
    /// The index of the picker's cursor within the current matches. Pass it to
    /// [`Picker::with_initial_cursor`] to reopen the picker where the user left
    /// it.
    pub cursor: u32,
}

/// A handler for one of the extra keys registered with
/// [`Picker::with_key_handlers`].
pub type PickerKeyHandler<T, D> = Box<dyn Fn(&mut Context, PickerAction<'_, T, D>)>;

/// The extra key bindings of a picker, keyed by the key event that triggers them.
pub type PickerKeyHandlers<T, D> = HashMap<KeyEvent, PickerKeyHandler<T, D>>;

#[cfg(test)]
mod modal_test {
    use super::*;
    use crate::ui::editor::canonicalize_key;

    /// The action a key triggers in normal mode, after the same
    /// canonicalization the picker applies to an incoming key.
    fn action(key: &str) -> Option<NormalModeAction> {
        let mut event: KeyEvent = key.parse().unwrap();
        canonicalize_key(&mut event);
        normal_mode_action(event)
    }

    #[test]
    fn normal_mode_keymap() {
        assert_eq!(action("j"), Some(NormalModeAction::MoveNext));
        assert_eq!(action("k"), Some(NormalModeAction::MovePrevious));
        assert_eq!(action("g"), Some(NormalModeAction::ToStart));
        assert_eq!(action("G"), Some(NormalModeAction::ToEnd));
        assert_eq!(action("J"), Some(NormalModeAction::ScrollPreviewLineDown));
        assert_eq!(action("K"), Some(NormalModeAction::ScrollPreviewLineUp));
        assert_eq!(action("f"), Some(NormalModeAction::ScrollPreviewPageDown));
        assert_eq!(action("b"), Some(NormalModeAction::ScrollPreviewPageUp));
        assert_eq!(action("t"), Some(NormalModeAction::TogglePreview));
        assert_eq!(action("v"), Some(NormalModeAction::OpenVerticalSplit));
        assert_eq!(action("s"), Some(NormalModeAction::OpenHorizontalSplit));
        assert_eq!(action("q"), Some(NormalModeAction::Close));
        for key in ["i", "a", "/"] {
            assert_eq!(action(key), Some(NormalModeAction::EnterInsertMode));
        }
    }

    #[test]
    fn uppercase_keys_match_with_or_without_the_shift_modifier() {
        // A terminal speaking the Kitty keyboard protocol reports `G` as
        // `Shift-G`; both spellings must reach the same binding.
        assert_eq!(action("G"), action("S-G"));
        assert_eq!(action("J"), action("S-J"));
        assert_eq!(action("K"), action("S-K"));
    }

    #[test]
    fn unbound_normal_mode_keys_are_not_query_text() {
        // These fall through to the picker's shared key handling, whose
        // fallback arm swallows them in normal mode instead of typing them into
        // the query.
        for key in ["x", "z", "1", "-", "%"] {
            assert_eq!(action(key), None, "{key} should have no normal-mode action");
        }

        // `Esc` and `Enter` deliberately have no entry of their own: the shared
        // handling closes and opens with them in both modes.
        assert_eq!(action("esc"), None);
        assert_eq!(action("ret"), None);

        // Neither do the `Ctrl-*` chords, which keep working identically in
        // both modes.
        for key in ["C-s", "C-v", "C-t", "C-n", "C-p", "C-d", "C-u", "C-c"] {
            assert_eq!(action(key), None, "{key} should be left to shared handling");
        }
    }

    #[test]
    fn modal_key_handlers_do_not_collide_with_the_normal_mode_keymap() {
        // The file explorer binds these bare keys through
        // `with_modal_key_handlers`, which is consulted before the keymap
        // above; if one were also a built-in normal-mode key the binding would
        // be unreachable.
        for key in ["m", "r", "d", "y", "p", "D", "Y"] {
            assert_eq!(action(key), None, "{key} is claimed twice");
        }

        // `a` is the deliberate exception: the explorer binds it to "create", a
        // yazi habit, and so shadows one of the three keys that enter insert
        // mode. The other two still reach the query.
        assert_eq!(action("a"), Some(NormalModeAction::EnterInsertMode));
        assert_eq!(action("i"), Some(NormalModeAction::EnterInsertMode));
        assert_eq!(action("/"), Some(NormalModeAction::EnterInsertMode));
    }

    #[test]
    fn a_picker_opens_in_insert_mode() {
        assert_eq!(PickerMode::default(), PickerMode::Insert);
    }
}
