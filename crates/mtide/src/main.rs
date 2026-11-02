use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    terminal,
};
use mtide_api::ColorSlot;
use mtide_core::{
    convert_identifier, expand_snippet, DocId, Document, Edit, NamingStyle, NavigationLocation,
    NavigationStack, Selection,
};
use mtide_diff::{apply_hunks, DiffOp, LineDiff};
use mtide_git::{
    Branch, ChangeKind as GitChangeKind, CommitSummary, GitCancellation, Repository, StashEntry,
    StatusEntry,
};
use mtide_search::{CancellationToken, SearchMatch, SearchOptions};
use mtide_task::{ResultStatus, TaskContext, TaskRun, TaskSpec, TaskState};
use mtide_term::{
    extract_links, KeyStroke, RoutedInput, TerminalLinkTarget, TerminalPanel, TerminalSession,
};
use mtide_tui::{
    discover_theme_packages, install_panic_hook, layout_with_sidebar, load_theme_package,
    render_with_options, wrap_line, Action, Cell, CellBuffer, ClipboardHistory, ColorCapability,
    CommandEntry, CommandRegistry, Keymap, TerminalCapabilities, TerminalGuard, ThemePackage,
    TuiError,
};
use mtide_vt::Color as VtColor;
use mtide_workspace::{
    atomic_write, changed_since, decode_text, encode_text, export_diagnostics, fingerprint,
    infer_indentation, load_config_layers, load_editorconfig, load_session, normalize_line_endings,
    read_file_cancellable, redacted_path, save_session, ChangeKind, DiagnosticReport,
    FileFingerprint, LineEnding, SessionSnapshot, SessionTab, TextEncoding, WorkspaceError,
    WorkspaceFile, WorkspaceWatcher,
};
use signal_hook::{consts::SIGINT, flag};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc,
};
use std::{
    collections::BTreeSet,
    env, fs, io,
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Error)]
enum AppError {
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("TUI failed: {0}")]
    Tui(#[from] TuiError),
    #[error("editing failed: {0}")]
    Edit(#[from] mtide_core::EditError),
    #[error("workspace failed: {0}")]
    Workspace(#[from] mtide_workspace::WorkspaceError),
    #[error("binary file cannot be edited as text: {0}")]
    Binary(PathBuf),
    #[error("file changed outside mtide; save was refused: {0}")]
    ExternalChange(PathBuf),
    #[error("file exceeds the 256 MiB safety limit: {0}")]
    TooLarge(PathBuf),
    #[error("terminal failed: {0}")]
    Terminal(#[from] mtide_term::TerminalError),
    #[error("task failed: {0}")]
    Task(#[from] mtide_task::TaskError),
    #[error("git failed: {0}")]
    Git(#[from] mtide_git::GitError),
}

type GitStatusResult = (
    PathBuf,
    Option<(
        String,
        (usize, usize, usize, usize),
        Vec<StatusEntry>,
        Vec<Branch>,
        Vec<CommitSummary>,
        Vec<StashEntry>,
        Option<String>,
        usize,
        bool,
    )>,
);
type WorkspaceSearchResult = (Vec<SearchMatch>, Option<String>);
type WorkspaceReplaceResult = (Vec<(PathBuf, Vec<u8>)>, usize, Vec<PathBuf>, Option<String>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitDiffLineKind {
    Equal,
    Added,
    Removed,
}

type GitDiffResult = (PathBuf, Option<LineDiff>, Option<String>);
type GitRemoteResult = (PathBuf, &'static str, Result<String, mtide_git::GitError>);
type GitBlameResult = (PathBuf, usize, Option<String>);

const MAX_EXTRA_CURSORS: usize = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitRemoteAction {
    Fetch,
    Pull,
    Push,
    Sync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitBranchAction {
    Merge,
    Rebase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitCheckoutMode {
    Prompt,
    Stash,
    Carry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitHistoryFilterKind {
    Keyword,
    Author,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConflictChoice {
    Ours,
    Theirs,
    Both,
    Neither,
}

type GitDiffRendered = (
    Vec<(String, GitDiffLineKind)>,
    Vec<Option<usize>>,
    Vec<usize>,
);

#[derive(Clone, Debug)]
struct EditorTab {
    path: Option<PathBuf>,
    text: String,
    cursor: usize,
    selection: Option<Selection>,
    extra_cursors: Vec<usize>,
    readonly: bool,
    baseline: Option<FileFingerprint>,
    encoding: TextEncoding,
    line_ending: LineEnding,
    large_file_mode: bool,
    folded_lines: BTreeSet<usize>,
    pinned: bool,
    preview: bool,
    dirty: bool,
}

#[derive(Clone, Debug)]
struct TaskOutputSnapshot {
    label: String,
    output: String,
    cwd: Option<PathBuf>,
}

struct App {
    document: Document,
    path: Option<PathBuf>,
    cursor: usize,
    selection: Option<Selection>,
    selection_history: Vec<Option<Selection>>,
    extra_cursors: Vec<usize>,
    clipboard: String,
    clipboard_history: ClipboardHistory,
    desired_column: usize,
    readonly: bool,
    session_path: PathBuf,
    search_query: String,
    search_matches: Vec<Range<usize>>,
    search_regex: bool,
    searching: bool,
    workspace_search_open: bool,
    workspace_search_query: String,
    workspace_search_regex: bool,
    workspace_search_case_sensitive: bool,
    workspace_search_whole_word: bool,
    workspace_search_results: Vec<SearchMatch>,
    workspace_search_error: Option<String>,
    workspace_search_selected: usize,
    workspace_search_job: Option<Receiver<WorkspaceSearchResult>>,
    workspace_search_token: Option<CancellationToken>,
    workspace_replace_mode: bool,
    workspace_replace_value: String,
    workspace_replace_job: Option<Receiver<WorkspaceReplaceResult>>,
    workspace_replace_token: Option<CancellationToken>,
    workspace_replace_snapshots: Vec<(PathBuf, Vec<u8>)>,
    baseline: Option<FileFingerprint>,
    word_wrap: bool,
    sticky_context: bool,
    theme: String,
    theme_package: Option<ThemePackage>,
    theme_choices: Vec<String>,
    theme_preview: Option<String>,
    keymap_preset: String,
    keymap: Keymap,
    vscode_chord_started: Option<Instant>,
    keymap_import_path: Option<PathBuf>,
    config_warning: bool,
    tab_width: u8,
    insert_spaces: bool,
    trim_trailing_whitespace: bool,
    insert_final_newline: bool,
    encoding: TextEncoding,
    line_ending: LineEnding,
    wrap_width: usize,
    horizontal_scroll: usize,
    large_file_mode: bool,
    watcher: Option<WorkspaceWatcher>,
    external_change: bool,
    notification: Option<String>,
    notification_history: Vec<String>,
    notifications_open: bool,
    last_recorded_notification: Option<String>,
    last_watch_poll: Instant,
    navigation: NavigationStack,
    folded_lines: BTreeSet<usize>,
    bookmarks: BTreeSet<usize>,
    workspace_roots: Vec<PathBuf>,
    snippet_stops: Vec<(usize, usize)>,
    snippet_index: usize,
    terminal_panel: TerminalPanel,
    terminal_selection: Option<((usize, usize), (usize, usize))>,
    mouse_selection_anchor: Option<usize>,
    zen_mode: bool,
    editor_split: bool,
    editor_split_horizontal: bool,
    tabs: Vec<EditorTab>,
    active_tab: usize,
    terminal_scrollback: usize,
    scrollbar_mode: String,
    native_colors: bool,
    transparent_background: bool,
    file_icons: bool,
    autosave_interval: Duration,
    last_autosave: Instant,
    save_all_on_quit: bool,
    commands: CommandRegistry,
    command_palette: bool,
    command_query: String,
    command_selected: usize,
    key_help: bool,
    key_help_query: String,
    recent_files: Vec<PathBuf>,
    recent_open: bool,
    recent_selected: usize,
    recent_query: String,
    file_finder_open: bool,
    file_finder_selected: usize,
    file_finder_query: String,
    file_finder_results: Vec<PathBuf>,
    file_finder_job: Option<Receiver<Result<Vec<PathBuf>, mtide_search::SearchError>>>,
    file_finder_token: Option<CancellationToken>,
    workspace_file: Option<PathBuf>,
    workspace_root_input_open: bool,
    workspace_root_input_remove: bool,
    workspace_root_input: String,
    terminal_rename_open: bool,
    terminal_rename_input: String,
    save_as_open: bool,
    save_as_input: String,
    sidebar_width: Option<u16>,
    sidebar_dragging: bool,
    task_run: Option<TaskRun>,
    background_tasks: Vec<(TaskRun, Instant)>,
    task_started: Option<Instant>,
    task_queue: Vec<TaskSpec>,
    task_output_open: bool,
    task_output_search: Option<String>,
    task_output_scroll: usize,
    task_output_tab: usize,
    task_output_history: Vec<TaskOutputSnapshot>,
    task_results_open: bool,
    task_result_selected: usize,
    git_repository: Option<Repository>,
    conflict_snapshots: std::collections::BTreeMap<PathBuf, String>,
    git_branch: Option<String>,
    git_counts: (usize, usize, usize, usize),
    git_entries: Vec<StatusEntry>,
    git_panel_open: bool,
    git_tree_mode: bool,
    git_selected: usize,
    git_branches: Vec<Branch>,
    git_branch_mode: bool,
    git_branch_selected: usize,
    git_history: Vec<CommitSummary>,
    git_history_mode: bool,
    git_history_selected: usize,
    git_history_offset: usize,
    git_history_snapshot: Option<String>,
    git_history_requested: bool,
    git_history_filter: String,
    git_history_filter_kind: GitHistoryFilterKind,
    git_history_filter_open: bool,
    git_history_file_only: bool,
    git_history_detail_open: bool,
    git_history_detail: Option<String>,
    git_history_detail_scroll: usize,
    git_history_detail_job: Option<Receiver<(PathBuf, Result<String, mtide_git::GitError>)>>,
    git_history_detail_cancel: Option<GitCancellation>,
    git_stashes: Vec<StashEntry>,
    git_stash_mode: bool,
    git_stash_selected: usize,
    git_commit_open: bool,
    git_commit_message: String,
    git_commit_amend: bool,
    git_commit_feedback: String,
    git_branch_input_open: bool,
    git_branch_input_rename: bool,
    git_branch_input: String,
    git_branch_delete_pending: Option<String>,
    git_checkout_pending: Option<String>,
    git_diff_open: bool,
    git_diff_path: Option<PathBuf>,
    git_diff_lines: Vec<(String, GitDiffLineKind)>,
    git_diff_line_hunks: Vec<Option<usize>>,
    git_diff_data: Option<LineDiff>,
    git_diff_hunks: Vec<usize>,
    git_diff_hunk_selected: usize,
    git_diff_selected_lines: BTreeSet<usize>,
    git_diff_scroll: usize,
    git_diff_split: bool,
    git_diff_ignore_whitespace: bool,
    git_diff_staged: bool,
    git_diff_context: usize,
    git_diff_job: Option<Receiver<GitDiffResult>>,
    git_diff_cancel: Option<GitCancellation>,
    git_editor_diff_mode: bool,
    git_editor_diff_snapshot: Option<String>,
    git_editor_original: Option<String>,
    git_editor_original_open: bool,
    git_editor_original_scroll: usize,
    git_editor_line_marks: Vec<GitDiffLineKind>,
    git_remote_job: Option<Receiver<GitRemoteResult>>,
    git_remote_cancel: Option<GitCancellation>,
    git_blame_job: Option<Receiver<GitBlameResult>>,
    git_blame_cancel: Option<GitCancellation>,
    git_blame_request: Option<(PathBuf, usize)>,
    git_blame: Option<String>,
    last_git_poll: Instant,
    git_status_job: Option<Receiver<GitStatusResult>>,
    git_status_cancel: Option<GitCancellation>,
}

impl App {
    #[cfg(test)]
    fn open(
        path: Option<PathBuf>,
        readonly: bool,
        stdin_text: Option<String>,
        session_path: PathBuf,
        initial_position: Option<(usize, usize)>,
        theme_override: Option<String>,
    ) -> Result<Self, AppError> {
        let cancelled = AtomicBool::new(false);
        Self::open_with_cancel(
            path,
            readonly,
            stdin_text,
            session_path,
            initial_position,
            theme_override,
            &cancelled,
        )
    }

    fn open_with_cancel(
        path: Option<PathBuf>,
        readonly: bool,
        stdin_text: Option<String>,
        session_path: PathBuf,
        initial_position: Option<(usize, usize)>,
        theme_override: Option<String>,
        cancelled: &AtomicBool,
    ) -> Result<Self, AppError> {
        let restored = if path.is_none() && stdin_text.is_none() {
            load_session(&session_path).ok()
        } else {
            None
        };
        let restored_path = restored.as_ref().and_then(|snapshot| snapshot.path.clone());
        let active_path = path.clone().or_else(|| restored_path.clone());
        let restored_dirty = restored.as_ref().is_some_and(|snapshot| {
            active_path.as_deref().is_some_and(|file| {
                fs::read(file)
                    .ok()
                    .and_then(|bytes| decode_text(&bytes))
                    .map(|(_, disk_text)| disk_text != snapshot.text)
                    .unwrap_or(true)
            })
        });
        let (text, encoding) = match path.as_deref() {
            Some(path)
                if path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "pdf" | "wasm")
                    }) =>
            {
                return Err(AppError::Binary(path.to_path_buf()))
            }
            Some(path)
                if fs::metadata(path)
                    .map(|metadata| metadata.len() > 256 * 1024 * 1024)
                    .unwrap_or(false) =>
            {
                return Err(AppError::TooLarge(path.to_path_buf()))
            }
            Some(path) => match read_file_cancellable(path, 256 * 1024 * 1024, cancelled) {
                Ok(bytes) => decode_text(&bytes)
                    .map(|(encoding, text)| (text, encoding))
                    .ok_or_else(|| AppError::Binary(path.to_path_buf()))?,
                Err(WorkspaceError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    (String::new(), TextEncoding::Utf8)
                }
                Err(WorkspaceError::FileTooLarge) => {
                    return Err(AppError::TooLarge(path.to_path_buf()))
                }
                Err(error) => return Err(error.into()),
            },
            None => (
                restored
                    .as_ref()
                    .map(|snapshot| snapshot.text.clone())
                    .or(stdin_text)
                    .unwrap_or_default(),
                TextEncoding::Utf8,
            ),
        };
        if text.contains('\0') {
            if let Some(path) = path.as_ref() {
                return Err(AppError::Binary(path.clone()));
            }
        }
        let line_ending = mtide_workspace::detect_line_ending(&text);
        let large_file_mode = text.len() >= 20 * 1024 * 1024 || text.lines().count() >= 500_000;
        let cursor_document = Document::new(DocId(99), &text);
        let cursor = initial_position
            .and_then(|(line, column)| {
                cursor_document
                    .line_column_to_byte(line.saturating_sub(1), column.saturating_sub(1))
            })
            .or_else(|| {
                restored.as_ref().and_then(|snapshot| {
                    cursor_document
                        .line_column_to_byte(snapshot.cursor_line, snapshot.cursor_column)
                })
            })
            .unwrap_or(0)
            .min(text.len());
        let project_config = active_path
            .as_deref()
            .and_then(find_project_config)
            .or_else(|| {
                env::current_dir()
                    .ok()
                    .map(|root| root.join(".mtide").join("config.toml"))
            })
            .unwrap_or_else(|| PathBuf::from(".mtide/config.toml"));
        let user_config = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".config").join("mtide").join("config.toml"));
        let config_paths = user_config
            .as_deref()
            .into_iter()
            .chain(std::iter::once(project_config.as_path()))
            .collect::<Vec<_>>();
        let (config, config_errors) = load_config_layers(&config_paths);
        let selected_theme = theme_override
            .clone()
            .unwrap_or_else(|| config.theme.clone());
        let theme_package = discover_theme_package(&selected_theme);
        let mut theme_choices = vec!["retro".to_owned(), "modern".to_owned()];
        theme_choices.extend(
            discover_user_theme_packages()
                .into_iter()
                .map(|(name, _)| name),
        );
        if !theme_choices
            .iter()
            .any(|theme| theme.eq_ignore_ascii_case(&selected_theme))
        {
            theme_choices.push(selected_theme.clone());
        }
        let editorconfig = active_path
            .as_deref()
            .and_then(find_editorconfig)
            .map(|file| load_editorconfig(&file))
            .unwrap_or_default();
        let inferred_indentation = infer_indentation(&text);
        let filesystem_readonly = active_path.as_deref().is_some_and(|file| {
            fs::metadata(file)
                .map(|metadata| metadata.permissions().readonly())
                .unwrap_or(false)
        });
        let baseline = active_path
            .as_deref()
            .and_then(|file| fingerprint(file).ok());
        let watcher = active_path.as_deref().and_then(|file| {
            file.parent()
                .and_then(|parent| WorkspaceWatcher::new(parent).ok())
        });
        let mut app = Self {
            document: Document::new(DocId(1), &text),
            path: active_path.clone(),
            cursor,
            selection: None,
            selection_history: Vec::new(),
            extra_cursors: Vec::new(),
            clipboard: String::new(),
            clipboard_history: ClipboardHistory::new(20),
            desired_column: 0,
            readonly: readonly || filesystem_readonly,
            session_path,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_regex: false,
            searching: false,
            workspace_search_open: false,
            workspace_search_query: String::new(),
            workspace_search_regex: false,
            workspace_search_case_sensitive: true,
            workspace_search_whole_word: false,
            workspace_search_results: Vec::new(),
            workspace_search_error: None,
            workspace_search_selected: 0,
            workspace_search_job: None,
            workspace_search_token: None,
            workspace_replace_mode: false,
            workspace_replace_value: String::new(),
            workspace_replace_job: None,
            workspace_replace_token: None,
            workspace_replace_snapshots: Vec::new(),
            baseline,
            word_wrap: config.word_wrap,
            sticky_context: true,
            theme: selected_theme,
            theme_package,
            theme_choices,
            theme_preview: None,
            keymap_preset: config.keymap.clone(),
            keymap: Keymap::preset(&config.keymap),
            vscode_chord_started: None,
            keymap_import_path: None,
            config_warning: !config_errors.is_empty(),
            tab_width: editorconfig
                .tab_width
                .or_else(|| inferred_indentation.map(|(_, width)| width))
                .unwrap_or(config.tab_width)
                .max(1),
            insert_spaces: editorconfig
                .insert_spaces
                .or_else(|| inferred_indentation.map(|(spaces, _)| spaces))
                .unwrap_or(config.insert_spaces),
            trim_trailing_whitespace: editorconfig.trim_trailing_whitespace,
            insert_final_newline: editorconfig.insert_final_newline,
            encoding,
            line_ending,
            wrap_width: 1,
            horizontal_scroll: 0,
            large_file_mode,
            watcher,
            external_change: false,
            notification: None,
            notification_history: Vec::new(),
            notifications_open: false,
            last_recorded_notification: None,
            last_watch_poll: Instant::now(),
            navigation: NavigationStack::default(),
            folded_lines: BTreeSet::new(),
            bookmarks: BTreeSet::new(),
            workspace_roots: path
                .as_ref()
                .and_then(|file| file.parent().map(Path::to_path_buf))
                .into_iter()
                .collect(),
            snippet_stops: Vec::new(),
            snippet_index: 0,
            terminal_panel: {
                let mut panel = TerminalPanel::default();
                let keys = config
                    .terminal_force_keys
                    .iter()
                    .filter_map(|key| parse_force_key(key))
                    .collect::<Vec<_>>();
                if !keys.is_empty() {
                    panel.key_policy.set_force_keys(keys);
                }
                panel
            },
            terminal_selection: None,
            mouse_selection_anchor: None,
            zen_mode: false,
            editor_split: false,
            editor_split_horizontal: false,
            tabs: Vec::new(),
            active_tab: 0,
            terminal_scrollback: config.terminal_scrollback,
            scrollbar_mode: config.scrollbar_mode,
            native_colors: config.native_colors,
            transparent_background: config.transparent_background,
            file_icons: config.file_icons,
            autosave_interval: Duration::from_secs(config.autosave_interval_secs),
            last_autosave: Instant::now(),
            save_all_on_quit: config.save_all_on_quit,
            commands: default_commands(),
            command_palette: false,
            command_query: String::new(),
            command_selected: 0,
            key_help: false,
            key_help_query: String::new(),
            recent_files: Vec::new(),
            recent_open: false,
            recent_selected: 0,
            recent_query: String::new(),
            file_finder_open: false,
            file_finder_selected: 0,
            file_finder_query: String::new(),
            file_finder_results: Vec::new(),
            file_finder_job: None,
            file_finder_token: None,
            workspace_file: None,
            workspace_root_input_open: false,
            workspace_root_input_remove: false,
            workspace_root_input: String::new(),
            terminal_rename_open: false,
            terminal_rename_input: String::new(),
            save_as_open: false,
            save_as_input: String::new(),
            sidebar_width: None,
            sidebar_dragging: false,
            task_run: None,
            background_tasks: Vec::new(),
            task_started: None,
            task_queue: Vec::new(),
            task_output_open: false,
            task_output_search: None,
            task_output_scroll: 0,
            task_output_tab: 0,
            task_output_history: Vec::new(),
            task_results_open: false,
            task_result_selected: 0,
            git_repository: active_path.as_deref().and_then(repository_for_path),
            conflict_snapshots: active_path
                .as_ref()
                .filter(|_| text.contains("<<<<<<<"))
                .map(|path| std::iter::once((path.clone(), text.clone())).collect())
                .unwrap_or_default(),
            git_branch: None,
            git_counts: (0, 0, 0, 0),
            git_entries: Vec::new(),
            git_panel_open: false,
            git_tree_mode: false,
            git_selected: 0,
            git_branches: Vec::new(),
            git_branch_mode: false,
            git_branch_selected: 0,
            git_history: Vec::new(),
            git_history_mode: false,
            git_history_selected: 0,
            git_history_offset: 0,
            git_history_snapshot: None,
            git_history_requested: true,
            git_history_filter: String::new(),
            git_history_filter_kind: GitHistoryFilterKind::Keyword,
            git_history_filter_open: false,
            git_history_file_only: false,
            git_history_detail_open: false,
            git_history_detail: None,
            git_history_detail_scroll: 0,
            git_history_detail_job: None,
            git_history_detail_cancel: None,
            git_stashes: Vec::new(),
            git_stash_mode: false,
            git_stash_selected: 0,
            git_commit_open: false,
            git_commit_message: String::new(),
            git_commit_amend: false,
            git_commit_feedback: String::new(),
            git_branch_input_open: false,
            git_branch_input_rename: false,
            git_branch_input: String::new(),
            git_branch_delete_pending: None,
            git_checkout_pending: None,
            git_diff_open: false,
            git_diff_path: None,
            git_diff_lines: Vec::new(),
            git_diff_line_hunks: Vec::new(),
            git_diff_data: None,
            git_diff_hunks: Vec::new(),
            git_diff_hunk_selected: 0,
            git_diff_selected_lines: BTreeSet::new(),
            git_diff_scroll: 0,
            git_diff_split: false,
            git_diff_ignore_whitespace: false,
            git_diff_staged: false,
            git_diff_context: 3,
            git_diff_job: None,
            git_diff_cancel: None,
            git_editor_diff_mode: false,
            git_editor_diff_snapshot: None,
            git_editor_original: None,
            git_editor_original_open: false,
            git_editor_original_scroll: 0,
            git_editor_line_marks: Vec::new(),
            git_remote_job: None,
            git_remote_cancel: None,
            git_blame_job: None,
            git_blame_cancel: None,
            git_blame_request: None,
            git_blame: None,
            last_git_poll: Instant::now(),
            git_status_job: None,
            git_status_cancel: None,
        };
        if restored_dirty {
            app.document.mark_dirty();
            app.notification = Some("restored unsaved session changes".into());
        }
        if let Some(path) = active_path.as_ref() {
            app.record_recent_file(path);
        }
        if let Some(snapshot) = restored {
            for root in snapshot.workspace_roots {
                if !app.workspace_roots.contains(&root) {
                    app.workspace_roots.push(root);
                }
            }
            app.folded_lines = snapshot.folded_lines.into_iter().collect();
            app.bookmarks = snapshot.bookmarks.into_iter().collect();
            for path in snapshot.recent_files.iter().rev() {
                app.record_recent_file(path);
            }
            if !snapshot.tabs.is_empty() {
                let active_tab = snapshot.active_tab;
                app.tabs = snapshot
                    .tabs
                    .into_iter()
                    .map(App::editor_tab_from_session)
                    .collect();
                app.active_tab = active_tab.min(app.tabs.len().saturating_sub(1));
                if let Some(tab) = app.tabs.get(app.active_tab).cloned() {
                    app.restore_editor_tab(tab);
                }
            }
        }
        app.ensure_tab_state();
        Ok(app)
    }

    fn current_editor_tab(&self) -> EditorTab {
        let (pinned, preview) = self
            .tabs
            .get(self.active_tab)
            .map(|tab| (tab.pinned, tab.preview))
            .unwrap_or((true, false));
        EditorTab {
            path: self.path.clone(),
            text: self.document.text(),
            cursor: self.cursor,
            selection: self.selection,
            extra_cursors: self.extra_cursors.clone(),
            readonly: self.readonly,
            baseline: self.baseline.clone(),
            encoding: self.encoding,
            line_ending: self.line_ending,
            large_file_mode: self.large_file_mode,
            folded_lines: self.folded_lines.clone(),
            pinned,
            preview,
            dirty: self.document.is_dirty(),
        }
    }

    fn ensure_tab_state(&mut self) {
        if self.tabs.is_empty() {
            self.tabs.push(self.current_editor_tab());
            self.active_tab = 0;
        }
    }

    fn sync_current_tab(&mut self) {
        self.ensure_tab_state();
        let current = self.current_editor_tab();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = current.path;
            tab.text = current.text;
            tab.cursor = current.cursor;
            tab.selection = current.selection;
            tab.extra_cursors = current.extra_cursors;
            tab.readonly = current.readonly;
            tab.baseline = current.baseline;
            tab.encoding = current.encoding;
            tab.line_ending = current.line_ending;
            tab.large_file_mode = current.large_file_mode;
            tab.folded_lines = current.folded_lines;
            tab.dirty = current.dirty;
        }
    }

    fn editor_tab_from_session(tab: SessionTab) -> EditorTab {
        let baseline = tab.path.as_deref().and_then(|path| fingerprint(path).ok());
        let readonly = tab.path.as_deref().is_some_and(|path| {
            fs::metadata(path)
                .map(|metadata| metadata.permissions().readonly())
                .unwrap_or(false)
        });
        let large_file_mode =
            tab.text.len() >= 20 * 1024 * 1024 || tab.text.lines().count() >= 500_000;
        let document = Document::new(DocId(1), &tab.text);
        let cursor = document
            .line_column_to_byte(tab.cursor_line, tab.cursor_column)
            .unwrap_or(0);
        EditorTab {
            path: tab.path,
            text: tab.text,
            cursor,
            selection: None,
            extra_cursors: Vec::new(),
            readonly,
            baseline,
            encoding: TextEncoding::Utf8,
            line_ending: LineEnding::Lf,
            large_file_mode,
            folded_lines: tab.folded_lines.into_iter().collect(),
            pinned: tab.pinned,
            preview: tab.preview,
            dirty: tab.dirty,
        }
    }

    fn restore_editor_tab(&mut self, tab: EditorTab) {
        if let Some(cancellation) = &self.git_diff_cancel {
            cancellation.cancel();
        }
        self.git_editor_diff_mode = false;
        self.git_editor_diff_snapshot = None;
        self.git_editor_original = None;
        self.git_editor_original_open = false;
        self.git_editor_original_scroll = 0;
        self.git_editor_line_marks.clear();
        self.path = tab.path.clone();
        if let Some(path) = self.path.clone() {
            if tab.text.contains("<<<<<<<")
                && tab.text.contains("=======")
                && tab.text.contains(">>>>>>>")
            {
                self.conflict_snapshots
                    .entry(path)
                    .or_insert_with(|| tab.text.clone());
            }
        }
        self.document = Document::new(DocId(1), &tab.text);
        if tab.dirty {
            self.document.mark_dirty();
        }
        self.cursor = tab.cursor.min(tab.text.len());
        self.selection = tab.selection;
        self.extra_cursors = tab.extra_cursors;
        self.readonly = tab.readonly;
        self.baseline = tab.baseline;
        self.encoding = tab.encoding;
        self.line_ending = tab.line_ending;
        self.large_file_mode = tab.large_file_mode;
        self.folded_lines = tab.folded_lines;
        self.watcher = self
            .path
            .as_deref()
            .and_then(Path::parent)
            .and_then(|parent| WorkspaceWatcher::new(parent).ok());
        self.git_repository = self.path.as_deref().and_then(repository_for_path);
        self.git_branch = None;
        self.git_counts = (0, 0, 0, 0);
        self.git_status_job = None;
        self.last_git_poll = Instant::now();
        self.external_change = false;
        self.snippet_stops.clear();
        self.snippet_index = 0;
        self.desired_column = self.current_column();
    }

    fn switch_tab(&mut self, delta: isize) {
        self.ensure_tab_state();
        self.sync_current_tab();
        let len = self.tabs.len() as isize;
        if len < 2 {
            return;
        }
        let next = (self.active_tab as isize + delta).rem_euclid(len) as usize;
        self.active_tab = next;
        if let Some(tab) = self.tabs.get(next).cloned() {
            self.restore_editor_tab(tab);
            self.notification = Some(format!("tab {}/{}", next + 1, self.tabs.len()));
        }
    }

    fn close_tab(&mut self, force: bool) {
        self.ensure_tab_state();
        if self.document.is_dirty() && !force {
            self.notification = Some("tab has unsaved changes; use Ctrl+Shift+W to close".into());
            return;
        }
        if self.tabs.len() == 1 {
            self.notification = Some("cannot close the last tab".into());
            return;
        }
        self.sync_current_tab();
        self.tabs.remove(self.active_tab);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        if let Some(tab) = self.tabs.get(self.active_tab).cloned() {
            self.restore_editor_tab(tab);
        }
    }

    fn pin_current_tab(&mut self) {
        self.ensure_tab_state();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.pinned = true;
            tab.preview = false;
            self.notification = Some("tab pinned".into());
        }
    }

    fn read_editor_tab(path: &Path, pinned: bool, preview: bool) -> Result<EditorTab, AppError> {
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(extension, "png" | "jpg" | "jpeg" | "gif" | "pdf" | "wasm")
            })
        {
            return Err(AppError::Binary(path.to_path_buf()));
        }
        if fs::metadata(path)
            .map(|metadata| metadata.len() > 256 * 1024 * 1024)
            .unwrap_or(false)
        {
            return Err(AppError::TooLarge(path.to_path_buf()));
        }
        let cancelled = AtomicBool::new(false);
        let bytes = read_file_cancellable(path, 256 * 1024 * 1024, &cancelled)?;
        let (encoding, text) =
            decode_text(&bytes).ok_or_else(|| AppError::Binary(path.to_path_buf()))?;
        let readonly = fs::metadata(path)
            .map(|metadata| metadata.permissions().readonly())
            .unwrap_or(false);
        Ok(EditorTab {
            path: Some(path.to_path_buf()),
            text: text.clone(),
            cursor: 0,
            selection: None,
            extra_cursors: Vec::new(),
            readonly,
            baseline: fingerprint(path).ok(),
            encoding,
            line_ending: mtide_workspace::detect_line_ending(&text),
            large_file_mode: text.len() >= 20 * 1024 * 1024 || text.lines().count() >= 500_000,
            folded_lines: BTreeSet::new(),
            pinned,
            preview,
            dirty: false,
        })
    }

    fn open_file_tab(&mut self, path: &Path, preview: bool) -> Result<(), AppError> {
        let tab = Self::read_editor_tab(path, !preview, preview)?;
        self.ensure_tab_state();
        if let Some(index) = self
            .tabs
            .iter()
            .position(|entry| entry.path.as_deref() == Some(path))
        {
            if index != self.active_tab {
                self.sync_current_tab();
                self.active_tab = index;
                self.restore_editor_tab(self.tabs[index].clone());
            }
            return Ok(());
        }
        self.sync_current_tab();
        let replace_preview = preview
            && self.tabs[self.active_tab].preview
            && !self.document.is_dirty()
            && !self.tabs[self.active_tab].pinned;
        if replace_preview {
            self.tabs[self.active_tab] = tab.clone();
        } else {
            self.tabs.push(tab.clone());
            self.active_tab = self.tabs.len() - 1;
        }
        self.restore_editor_tab(tab);
        self.record_recent_file(path);
        self.notification = Some(format!("opened tab: {}", path.display()));
        Ok(())
    }

    fn record_recent_file(&mut self, path: &Path) {
        let path = path.to_path_buf();
        self.recent_files.retain(|entry| entry != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(20);
    }

    fn open_recent_files(&mut self) {
        self.recent_open = true;
        self.recent_selected = 0;
        self.recent_query.clear();
    }

    fn open_file_finder(&mut self) {
        self.file_finder_open = true;
        self.file_finder_selected = 0;
        self.file_finder_query.clear();
        self.file_finder_results.clear();
        self.start_file_finder_search();
    }

    fn start_file_finder_search(&mut self) {
        if let Some(token) = self.file_finder_token.take() {
            token.cancel();
        }
        let query = self.file_finder_query.clone();
        if query.trim().is_empty() {
            self.file_finder_job = None;
            self.file_finder_results.clear();
            return;
        }
        let mut roots = self.workspace_roots.clone();
        if roots.is_empty() {
            if let Some(parent) = self.path.as_deref().and_then(Path::parent) {
                roots.push(parent.to_path_buf());
            }
        }
        if roots.is_empty() {
            roots.push(PathBuf::from("."));
        }
        let token = CancellationToken::default();
        let worker_token = token.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = mtide_search::find_files_roots(&roots, &query, &worker_token);
            let _ = sender.send(result);
        });
        self.file_finder_token = Some(token);
        self.file_finder_job = Some(receiver);
    }

    fn poll_file_finder(&mut self) {
        let Some(job) = self.file_finder_job.as_ref() else {
            return;
        };
        let Ok(result) = job.try_recv() else {
            return;
        };
        self.file_finder_job = None;
        self.file_finder_token = None;
        match result {
            Ok(results) => {
                self.file_finder_results = results;
                self.file_finder_selected = self
                    .file_finder_selected
                    .min(self.file_finder_results.len().saturating_sub(1));
            }
            Err(error) => {
                self.file_finder_results.clear();
                self.notification = Some(format!("file search failed: {error}"));
            }
        }
    }

    fn filtered_recent_files(&self) -> Vec<PathBuf> {
        let query = self.recent_query.to_lowercase();
        self.recent_files
            .iter()
            .filter(|path| {
                query.is_empty() || path.to_string_lossy().to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }

    fn run(&mut self, interrupted: Arc<AtomicBool>) -> Result<(), AppError> {
        let _guard = TerminalGuard::enter()?;
        let mut previous = CellBuffer::new(0, 0);
        loop {
            self.poll_workspace_changes();
            self.poll_workspace_search();
            self.poll_workspace_replace();
            self.poll_file_finder();
            self.terminal_panel.sessions.poll_all();
            self.poll_task()?;
            self.poll_git()?;
            self.poll_autosave();
            let (width, height) = terminal::size()?;
            let fullscreen = self.terminal_panel.is_fullscreen();
            if let Some(session) = self.terminal_panel.sessions.selected_mut() {
                let terminal_height = if fullscreen { height as usize } else { 6 };
                session.resize(width as usize, terminal_height)?;
            }
            let mut frame = CellBuffer::new(width, height);
            self.draw(&mut frame, width, height);
            if previous.width != width || previous.height != height {
                previous = CellBuffer::new(width, height);
            }
            let mut stdout = io::stdout();
            render_with_options(
                &mut stdout,
                &frame,
                &previous,
                mtide_tui::RenderOptions {
                    inherit_terminal_colors: self.native_colors,
                    transparent_background: self.transparent_background,
                },
            )?;
            previous = frame;
            if interrupted.load(Ordering::Acquire) {
                break;
            }
            if event::poll(Duration::from_millis(8))? {
                match event::read()? {
                    Event::Key(key) if self.handle(key)? => break,
                    Event::Mouse(mouse) => self.handle_mouse(mouse, width, height),
                    Event::Paste(text) => self.handle_terminal_paste(&text)?,
                    _ => {}
                }
            }
        }
        self.shutdown()?;
        Ok(())
    }

    fn draw(&mut self, frame: &mut CellBuffer, width: u16, height: u16) {
        self.record_notification();
        if self.terminal_panel.is_fullscreen() && self.terminal_panel.sessions.selected().is_some()
        {
            frame.clear();
            self.draw_terminal_fullscreen(frame, width, height);
            return;
        }
        let geometry = layout_with_sidebar(width, height, !self.zen_mode, self.sidebar_width);
        let editor_x = geometry.sidebar.map(|(sidebar, _)| sidebar).unwrap_or(0);
        let retro_theme = self
            .theme_preview
            .as_deref()
            .and_then(discover_theme_package)
            .as_ref()
            .or(self.theme_package.as_ref())
            .map(|package| package.function_key_bar)
            .unwrap_or_else(|| {
                !self
                    .theme_preview
                    .as_deref()
                    .unwrap_or(&self.theme)
                    .eq_ignore_ascii_case("modern")
            });
        let top_rule = self
            .theme_preview
            .as_deref()
            .and_then(discover_theme_package)
            .as_ref()
            .or(self.theme_package.as_ref())
            .map(|package| package.border_horizontal.clone())
            .unwrap_or_else(|| (if retro_theme { "═" } else { " " }).into());
        self.ensure_tab_state();
        self.sync_current_tab();
        let tab_bar = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let title = tab
                    .path
                    .as_deref()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .unwrap_or("[untitled]");
                format!(
                    "{}{}{}{}{}",
                    if index == self.active_tab { "[" } else { " " },
                    if self.file_icons {
                        format!("[{}] ", file_icon(tab.path.as_deref()))
                    } else {
                        String::new()
                    },
                    title,
                    if tab.dirty { "*" } else { "" },
                    if index == self.active_tab { "]" } else { " " },
                )
            })
            .collect::<String>();
        frame.write_str(
            0,
            0,
            &format!(
                "{} mtide  |  Ctrl-S 保存  Ctrl-Q 退出  Ctrl-Tab 标签  {}{}{}",
                top_rule,
                tab_bar,
                if self.editor_split {
                    if self.editor_split_horizontal {
                        "  [split H]"
                    } else {
                        "  [split V]"
                    }
                } else {
                    ""
                },
                if self.git_editor_diff_mode {
                    "  [git diff edit]"
                } else {
                    ""
                }
            ),
            mtide_api::ColorSlot::Focus,
        );
        if let Some((sidebar_width, sidebar_height)) = geometry.sidebar {
            for row in 1..sidebar_height {
                frame.write_str(
                    sidebar_width.saturating_sub(1),
                    row,
                    "│",
                    mtide_api::ColorSlot::Muted,
                );
            }
            frame.write_str(1, 1, "WORKSPACE", mtide_api::ColorSlot::Focus);
            frame.write_str(1, 2, "▾ [workspace]", mtide_api::ColorSlot::Muted);
            for (index, root) in self.workspace_roots.iter().take(3).enumerate() {
                let label = root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("root");
                frame.write_str(
                    3,
                    3 + index as u16,
                    &format!("▸ {label}"),
                    mtide_api::ColorSlot::Default,
                );
            }
            let _ = sidebar_width;
        }
        if self.searching {
            frame.write_str(
                0,
                1,
                &format!(
                    " Find{}: {} ({} matches; Ctrl-Alt-F regex)",
                    if self.search_regex { " regex" } else { "" },
                    self.search_query,
                    self.search_matches.len()
                ),
                mtide_api::ColorSlot::Focus,
            );
        }
        let text = self.document.text();
        let lines: Vec<&str> = text.split('\n').collect();
        let top: u16 = if self.searching { 2 } else { 1 };
        // Keep the editor viewport stable across themes; Retro's key bar is a
        // chrome overlay on the final editor row rather than a layout change.
        let language = self
            .path
            .as_deref()
            .map(|path| mtide_syntax::detect_with_text(path, &text))
            .unwrap_or(mtide_syntax::Language::Rust);
        let sticky_context = if self.sticky_context && !self.large_file_mode {
            sticky_context_lines(&text, language, self.current_line(), 2)
        } else {
            Vec::new()
        };
        for (index, line) in sticky_context.iter().enumerate() {
            let content = lines.get(*line).copied().unwrap_or_default();
            frame.write_str(
                editor_x,
                top.saturating_add(index as u16),
                &format!("↳ {:>4} {}", line + 1, content),
                mtide_api::ColorSlot::Focus,
            );
        }
        let content_top = top.saturating_add(sticky_context.len() as u16);
        let content_height = height.saturating_sub(content_top + 1) as usize;
        let wrap_width = geometry.editor.0.saturating_sub(5) as usize;
        // Keep the width available to keyboard navigation as well as rendering.
        // A zero-width terminal is represented as one logical column.
        self.wrap_width = wrap_width.max(1);
        if !self.word_wrap || self.large_file_mode {
            self.horizontal_scroll = self
                .current_column()
                .saturating_sub(self.wrap_width.saturating_sub(1));
        }
        let mut visual_row = 0usize;
        let folds = if self.large_file_mode {
            Vec::new()
        } else {
            mtide_syntax::fold_regions(&text, language)
        };
        for (row, line) in lines.iter().enumerate() {
            if let Some(region) = folds
                .iter()
                .find(|region| region.start_line == row && self.folded_lines.contains(&row))
            {
                if visual_row < content_height {
                    frame.write_str(
                        editor_x,
                        visual_row as u16 + content_top,
                        &format!("{:>4} … folded …", row + 1),
                        mtide_api::ColorSlot::Muted,
                    );
                    if self.bookmarks.contains(&row) {
                        frame.write_str(
                            editor_x.saturating_add(4),
                            visual_row as u16 + content_top,
                            "◆",
                            mtide_api::ColorSlot::Focus,
                        );
                    }
                    visual_row += 1;
                }
                if region.end_line > row {
                    continue;
                }
            }
            if folds.iter().any(|region| {
                self.folded_lines.contains(&region.start_line)
                    && row > region.start_line
                    && row <= region.end_line
            }) {
                continue;
            }
            let wrapped_lines = if self.word_wrap && !self.large_file_mode {
                wrap_line(line, wrap_width)
            } else {
                vec![horizontal_view(line, self.horizontal_scroll, wrap_width)]
            };
            for (wrapped_index, wrapped) in wrapped_lines.into_iter().enumerate() {
                if visual_row >= content_height {
                    break;
                }
                let label = if wrapped_index == 0 {
                    format!("{:>4} ", row + 1)
                } else {
                    "     ".to_owned()
                };
                frame.write_str(
                    editor_x,
                    visual_row as u16 + content_top,
                    &label,
                    mtide_api::ColorSlot::Muted,
                );
                if wrapped_index == 0 {
                    let (marker, color) = match self.git_editor_line_marks.get(row) {
                        Some(GitDiffLineKind::Added) => ('+', ColorSlot::Success),
                        Some(GitDiffLineKind::Removed) => ('-', ColorSlot::Error),
                        _ => (' ', ColorSlot::Muted),
                    };
                    frame.write_str(
                        editor_x.saturating_add(4),
                        visual_row as u16 + content_top,
                        &marker.to_string(),
                        color,
                    );
                }
                if wrapped_index == 0 && self.bookmarks.contains(&row) {
                    frame.write_str(
                        editor_x.saturating_add(4),
                        visual_row as u16 + content_top,
                        "◆",
                        mtide_api::ColorSlot::Focus,
                    );
                }
                frame.write_str(
                    editor_x.saturating_add(5),
                    visual_row as u16 + content_top,
                    &wrapped,
                    mtide_api::ColorSlot::Default,
                );
                visual_row += 1;
            }
            if visual_row >= content_height {
                break;
            }
        }
        self.draw_editor_scrollbar(
            frame,
            editor_x,
            geometry.editor.0,
            content_top as usize,
            content_height,
            lines.len(),
        );
        if self.editor_split && geometry.editor.0 >= 80 {
            if self.editor_split_horizontal && content_height >= 8 {
                self.draw_editor_horizontal_split(
                    frame,
                    editor_x,
                    geometry.editor.0,
                    content_top as usize,
                    content_height,
                    &lines,
                );
            } else {
                self.draw_editor_split_pane(
                    frame,
                    editor_x,
                    geometry.editor.0,
                    content_top as usize,
                    content_height,
                    &lines,
                );
            }
        }
        let status = self.status_line(width as usize, text.len());
        frame.write_str(
            0,
            height.saturating_sub(1),
            &status,
            mtide_api::ColorSlot::Success,
        );
        if !self.zen_mode {
            self.draw_task_panel(frame, width, height);
            self.draw_terminal_panel(frame, width, height);
        }
        if self.command_palette {
            self.draw_command_palette(frame, width, height);
        }
        if self.workspace_search_open {
            self.draw_workspace_search(frame, width, height);
        }
        if self.key_help {
            self.draw_key_help(frame, width, height);
        }
        if self.recent_open {
            self.draw_recent_files(frame, width, height);
        }
        if self.file_finder_open {
            self.draw_file_finder(frame, width, height);
        }
        if self.notifications_open {
            self.draw_notification_history(frame, width, height);
        }
        if self.git_panel_open {
            self.draw_git_panel(frame, width, height);
        }
        if self.git_history_detail_open {
            self.draw_git_history_detail(frame, width, height);
        }
        if self.git_editor_original_open {
            self.draw_git_editor_original(frame, width, height);
        }
        if self.git_commit_open {
            self.draw_git_commit(frame, width, height);
        }
        if self.git_branch_input_open {
            self.draw_git_branch_input(frame, width, height);
        }
        if self.workspace_root_input_open {
            self.draw_workspace_root_input(frame, width, height);
        }
        if self.terminal_rename_open {
            self.draw_terminal_rename(frame, width, height);
        }
        if self.save_as_open {
            self.draw_save_as(frame, width, height);
        }
        if self.git_diff_open {
            self.draw_git_diff(frame, width, height);
        }
        if self.task_results_open {
            self.draw_task_results(frame, width, height);
        }
        if self.task_output_open {
            self.draw_task_output(frame, width, height);
        }
        if let Some(theme) = &self.theme_preview {
            let message = format!(" Theme preview: {theme} (Enter apply, Esc cancel, ←/→ switch) ");
            frame.write_str(
                width.saturating_sub(message.len() as u16).saturating_sub(1),
                0,
                &message,
                ColorSlot::Focus,
            );
        }
        if retro_theme && height >= 2 {
            frame.write_str(
                0,
                height.saturating_sub(2),
                " F1 Help  F6 Problems  F7 Git  F8 Notices  F9 Build  Ctrl+` Terminal ",
                mtide_api::ColorSlot::Focus,
            );
        }
        let _ = width;
    }

    fn record_notification(&mut self) {
        let Some(notification) = self.notification.clone() else {
            return;
        };
        if self.last_recorded_notification.as_deref() == Some(notification.as_str()) {
            return;
        }
        self.notification_history.push(notification.clone());
        self.notification_history.truncate(20);
        self.last_recorded_notification = Some(notification);
    }

    fn draw_editor_scrollbar(
        &self,
        frame: &mut CellBuffer,
        editor_x: u16,
        editor_width: u16,
        top: usize,
        height: usize,
        line_count: usize,
    ) {
        if editor_width == 0 || height == 0 || self.scrollbar_mode == "hidden" {
            return;
        }
        let x = editor_x.saturating_add(editor_width.saturating_sub(1));
        let last_line = line_count.saturating_sub(1).max(1);
        let track = if self.scrollbar_mode == "block" {
            '█'
        } else {
            '│'
        };
        for row in 0..height {
            let y = top.saturating_add(row);
            let line = row.saturating_mul(last_line) / height.saturating_sub(1).max(1);
            let mut symbol = track;
            let mut style = ColorSlot::Muted;
            if self
                .search_matches
                .iter()
                .any(|range| self.document.byte_to_line(range.start) == Some(line))
            {
                symbol = '•';
                style = ColorSlot::Focus;
            }
            if self.bookmarks.contains(&line) {
                symbol = '◆';
                style = ColorSlot::Focus;
            }
            if self
                .git_editor_line_marks
                .get(line)
                .is_some_and(|mark| matches!(mark, GitDiffLineKind::Added))
            {
                symbol = '▌';
                style = ColorSlot::Success;
            }
            if self.document.byte_to_line(self.cursor) == Some(line) {
                symbol = '█';
                style = ColorSlot::Success;
            }
            frame.write_str(x, y as u16, &symbol.to_string(), style);
        }
    }

    fn draw_editor_split_pane(
        &self,
        frame: &mut CellBuffer,
        editor_x: u16,
        editor_width: u16,
        top: usize,
        height: usize,
        lines: &[&str],
    ) {
        let first_width = editor_width / 2;
        let right_x = editor_x.saturating_add(first_width);
        let right_width = editor_width.saturating_sub(first_width);
        if right_width < 20 {
            return;
        }
        for row in 0..height {
            frame.write_str(
                right_x.saturating_add(1),
                top.saturating_add(row) as u16,
                &" ".repeat(right_width.saturating_sub(1) as usize),
                ColorSlot::Default,
            );
        }
        for row in 0..height {
            frame.write_str(
                right_x,
                top.saturating_add(row) as u16,
                "│",
                ColorSlot::Muted,
            );
        }
        let content_width = right_width.saturating_sub(6) as usize;
        let mut visual_row = 0usize;
        for (line_number, line) in lines.iter().enumerate() {
            if visual_row >= height {
                break;
            }
            let wrapped = if self.word_wrap {
                wrap_line(line, content_width.max(1))
            } else {
                vec![horizontal_view(
                    line,
                    self.horizontal_scroll,
                    content_width.max(1),
                )]
            };
            for (wrapped_index, row_text) in wrapped.into_iter().enumerate() {
                if visual_row >= height {
                    break;
                }
                let label = if wrapped_index == 0 {
                    format!("{:>4} ", line_number + 1)
                } else {
                    "     ".to_owned()
                };
                frame.write_str(
                    right_x.saturating_add(1),
                    top.saturating_add(visual_row) as u16,
                    &label,
                    ColorSlot::Muted,
                );
                frame.write_str(
                    right_x.saturating_add(6),
                    top.saturating_add(visual_row) as u16,
                    &row_text,
                    ColorSlot::Default,
                );
                visual_row += 1;
            }
        }
    }

    fn draw_editor_horizontal_split(
        &self,
        frame: &mut CellBuffer,
        editor_x: u16,
        editor_width: u16,
        top: usize,
        height: usize,
        lines: &[&str],
    ) {
        let first_height = height / 2;
        let divider_y = top.saturating_add(first_height) as u16;
        let second_height = height.saturating_sub(first_height + 1);
        if second_height == 0 {
            return;
        }
        for row in 0..second_height {
            frame.write_str(
                editor_x,
                divider_y.saturating_add(1 + row as u16),
                &" ".repeat(editor_width as usize),
                ColorSlot::Default,
            );
        }
        frame.write_str(
            editor_x,
            divider_y,
            &"─".repeat(editor_width as usize),
            ColorSlot::Muted,
        );
        let content_width = editor_width.saturating_sub(6) as usize;
        for (visual_row, (line_number, line)) in
            lines.iter().enumerate().enumerate().take(second_height)
        {
            let row_text = if self.word_wrap {
                wrap_line(line, content_width.max(1))
                    .into_iter()
                    .next()
                    .unwrap_or_default()
            } else {
                horizontal_view(line, self.horizontal_scroll, content_width.max(1))
            };
            frame.write_str(
                editor_x,
                divider_y.saturating_add(1 + visual_row as u16),
                &format!("{:>4} ", line_number + 1),
                ColorSlot::Muted,
            );
            frame.write_str(
                editor_x.saturating_add(5),
                divider_y.saturating_add(1 + visual_row as u16),
                &row_text,
                ColorSlot::Default,
            );
        }
    }

    fn draw_recent_files(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let entries = self.filtered_recent_files();
        let box_width = width.saturating_sub(8).clamp(40, 90);
        let visible = entries.len().min(height.saturating_sub(4) as usize);
        let change_rows = if !self.git_stash_mode && !self.git_history_mode && !self.git_branch_mode
        {
            visible.saturating_mul(2)
        } else {
            visible
        };
        let box_height = (change_rows as u16 + 3)
            .max(3)
            .min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            &format!(" Recent files: {} (Esc to close) ", self.recent_query),
            ColorSlot::Focus,
        );
        if entries.is_empty() {
            frame.write_str(left + 2, top + 1, "(none)", ColorSlot::Default);
        }
        for (index, path) in entries.iter().take(visible).enumerate() {
            let marker = if index == self.recent_selected {
                ">"
            } else {
                " "
            };
            let availability = if path.exists() { "" } else { " [missing]" };
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!("{marker} {}{availability}", path.display()),
                if index == self.recent_selected {
                    ColorSlot::Focus
                } else {
                    ColorSlot::Default
                },
            );
        }
    }

    fn draw_file_finder(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(48, 100);
        let visible = self
            .file_finder_results
            .len()
            .min(height.saturating_sub(5) as usize);
        let box_height = (visible as u16 + 4).max(5).min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            &format!(
                " Files: {} (Enter open, Esc close){} ",
                self.file_finder_query,
                if self.file_finder_job.is_some() {
                    " searching…"
                } else {
                    ""
                }
            ),
            ColorSlot::Focus,
        );
        if self.file_finder_results.is_empty() && self.file_finder_job.is_none() {
            frame.write_str(left + 2, top + 1, "(no matching files)", ColorSlot::Muted);
        }
        for (index, path) in self.file_finder_results.iter().take(visible).enumerate() {
            let marker = if index == self.file_finder_selected {
                ">"
            } else {
                " "
            };
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!("{marker} {}", path.display()),
                if index == self.file_finder_selected {
                    ColorSlot::Focus
                } else {
                    ColorSlot::Default
                },
            );
        }
    }

    fn draw_notification_history(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(40, 82);
        let visible = self
            .notification_history
            .len()
            .min(height.saturating_sub(4) as usize);
        let box_height = (visible as u16 + 3).max(3).min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            " Notifications (Esc to close) ",
            ColorSlot::Focus,
        );
        for (index, message) in self
            .notification_history
            .iter()
            .rev()
            .take(visible)
            .enumerate()
        {
            let (marker, style) = notification_presentation(message);
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!("{marker} {message}"),
                style,
            );
        }
    }

    fn draw_git_panel(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(44, 96);
        let visible = if self.git_stash_mode {
            self.git_stashes
                .len()
                .min(height.saturating_sub(4) as usize)
        } else if self.git_history_mode {
            self.git_history
                .len()
                .min(height.saturating_sub(4) as usize)
        } else if self.git_branch_mode {
            self.git_branches
                .len()
                .min(height.saturating_sub(4) as usize)
        } else {
            self.git_entries
                .len()
                .min(height.saturating_sub(4) as usize)
        };
        let box_height = (visible as u16 + 3).max(3).min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        if self.git_stash_mode {
            frame.write_str(
                left + 1,
                top,
                " Git stash (n create, Enter view, p apply, o pop, d drop, Esc close) ",
                ColorSlot::Focus,
            );
            if self.git_stashes.is_empty() {
                frame.write_str(left + 2, top + 1, "(no stashes)", ColorSlot::Muted);
            }
            for (index, stash) in self.git_stashes.iter().take(visible).enumerate() {
                let marker = if index == self.git_stash_selected {
                    ">"
                } else {
                    " "
                };
                frame.write_str(
                    left + 1,
                    top + 1 + index as u16,
                    &format!("{marker} {} — {}", stash.reference, stash.message),
                    if index == self.git_stash_selected {
                        ColorSlot::Focus
                    } else {
                        ColorSlot::Default
                    },
                );
            }
        } else if self.git_history_mode {
            frame.write_str(
                left + 1,
                top,
                &format!(
                    " Git history (PgDn older, / keyword, a author, p file={}, Enter details, Esc close) {}{} ",
                    if self.git_history_file_only { "on" } else { "off" },
                    match self.git_history_filter_kind {
                        GitHistoryFilterKind::Keyword => "grep=",
                        GitHistoryFilterKind::Author => "author=",
                    },
                    self.git_history_filter
                ),
                ColorSlot::Focus,
            );
            if self.git_history.is_empty() {
                frame.write_str(left + 2, top + 1, "(no commits)", ColorSlot::Muted);
            }
            for (index, commit) in self.git_history.iter().take(visible).enumerate() {
                let marker = if index == self.git_history_selected {
                    ">"
                } else {
                    " "
                };
                let short_id = commit.id.chars().take(8).collect::<String>();
                frame.write_str(
                    left + 1,
                    top + 1 + index as u16,
                    &format!("{marker} {short_id} {} — {}", commit.author, commit.subject),
                    if index == self.git_history_selected {
                        ColorSlot::Focus
                    } else {
                        ColorSlot::Default
                    },
                );
            }
        } else if self.git_branch_mode {
            frame.write_str(
                left + 1,
                top,
                " Git branches (Enter checkout; s stash/carry prompt; m merge, R rebase, n new, r rename, d delete, b changes, Esc close) ",
                ColorSlot::Focus,
            );
            if self.git_branches.is_empty() {
                frame.write_str(left + 2, top + 1, "(no branches)", ColorSlot::Muted);
            }
            for (index, branch) in self.git_branches.iter().take(visible).enumerate() {
                let marker = if index == self.git_branch_selected {
                    ">"
                } else {
                    " "
                };
                let current = if branch.current { "*" } else { " " };
                let remote = if branch.remote { "remote" } else { "local" };
                frame.write_str(
                    left + 1,
                    top + 1 + index as u16,
                    &format!("{marker} {current} {} ({remote})", branch.name),
                    if index == self.git_branch_selected {
                        ColorSlot::Focus
                    } else {
                        ColorSlot::Default
                    },
                );
            }
        } else {
            frame.write_str(
                left + 1,
                top,
                &format!(
                    " Git changes (Space stage, t tree={}, f fetch, l pull, u push, y sync, b branches, h history, Esc close) ",
                    if self.git_tree_mode { "on" } else { "off" }
                ),
                ColorSlot::Focus,
            );
            if self.git_entries.is_empty() {
                frame.write_str(left + 2, top + 1, "(clean)", ColorSlot::Success);
            }
            let mut last_group = None;
            let mut last_directory: Option<PathBuf> = None;
            let mut row = 0_u16;
            for (index, entry) in self.git_entries.iter().take(visible).enumerate() {
                let marker = if index == self.git_selected { ">" } else { " " };
                let status = if entry.index_code != '.' && entry.index_code != ' ' {
                    entry.index_code
                } else {
                    entry.worktree_code
                };
                let kind = match entry.kind {
                    GitChangeKind::Untracked => "untracked",
                    GitChangeKind::Unmerged => "conflict",
                    GitChangeKind::Deleted => "deleted",
                    GitChangeKind::Added => "added",
                    _ => "modified",
                };
                let group = if matches!(entry.kind, GitChangeKind::Unmerged) {
                    "conflict"
                } else if entry.index_code != '.' && entry.index_code != ' ' {
                    "staged"
                } else if entry.index_code == '?' {
                    "untracked"
                } else {
                    "unstaged"
                };
                if last_group != Some(group) {
                    frame.write_str(
                        left + 2,
                        top + 1 + row,
                        &format!("▾ {group}"),
                        ColorSlot::Focus,
                    );
                    row = row.saturating_add(1);
                    last_group = Some(group);
                }
                if self.git_tree_mode {
                    let directory = entry.path.parent().map(Path::to_path_buf);
                    if directory != last_directory {
                        let label = directory
                            .as_deref()
                            .filter(|path| !path.as_os_str().is_empty())
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| ".".into());
                        frame.write_str(
                            left + 3,
                            top + 1 + row,
                            &format!("▸ {label}"),
                            ColorSlot::Muted,
                        );
                        row = row.saturating_add(1);
                        last_directory = directory;
                    }
                }
                let display_path = if self.git_tree_mode {
                    entry
                        .path
                        .file_name()
                        .map(|name| format!("  {}", name.to_string_lossy()))
                        .unwrap_or_else(|| entry.path.display().to_string())
                } else {
                    entry.path.display().to_string()
                };
                frame.write_str(
                    left + 1,
                    top + 1 + row,
                    &format!("{marker} [{status}] {group}/{kind}: {display_path}",),
                    if index == self.git_selected {
                        ColorSlot::Focus
                    } else if matches!(entry.kind, GitChangeKind::Unmerged) {
                        ColorSlot::Error
                    } else {
                        ColorSlot::Default
                    },
                );
                row = row.saturating_add(1);
            }
        }
    }

    fn draw_git_commit(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(48, 96);
        let message_lines = self.git_commit_message.lines().count().max(1) as u16;
        let feedback_lines = self.git_commit_feedback.lines().count() as u16;
        let box_height = message_lines
            .saturating_add(feedback_lines.saturating_add(4))
            .min(height.saturating_sub(2))
            .max(5);
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            &format!(
                " Commit staged changes ({}; Shift-Enter newline, Ctrl-A amend, Esc cancel) ",
                if self.git_commit_amend {
                    "amend on"
                } else {
                    "Enter commit"
                }
            ),
            ColorSlot::Focus,
        );
        for (index, line) in self.git_commit_message.lines().enumerate() {
            frame.write_str(
                left + 2,
                top + 2 + index as u16,
                &format!("> {line}"),
                ColorSlot::Default,
            );
        }
        frame.write_str(
            left + 2,
            top + 2 + message_lines,
            if self.git_commit_feedback.is_empty() {
                "Empty messages are rejected; hook output remains visible on failure."
            } else {
                "Commit hook output:"
            },
            ColorSlot::Muted,
        );
        for (index, line) in self.git_commit_feedback.lines().enumerate() {
            let row = top + 3 + message_lines + index as u16;
            if row < top + box_height {
                frame.write_str(left + 2, row, line, ColorSlot::Error);
            }
        }
    }

    fn draw_git_history_detail(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(4).clamp(64, 140);
        let box_height = height.saturating_sub(2).max(5);
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            " Git detail / stash or commit (Esc close) ",
            ColorSlot::Focus,
        );
        let lines = self
            .git_history_detail
            .as_deref()
            .unwrap_or("loading commit detail…")
            .lines()
            .collect::<Vec<_>>();
        let visible = box_height.saturating_sub(2) as usize;
        let start = self
            .git_history_detail_scroll
            .min(lines.len().saturating_sub(visible));
        for (index, line) in lines.iter().skip(start).take(visible).enumerate() {
            frame.write_str(left + 1, top + 1 + index as u16, line, ColorSlot::Default);
        }
    }

    fn draw_git_editor_original(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(4).clamp(56, 120);
        let box_height = height.saturating_sub(2).max(5);
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            " Git original (read-only, Esc close) ",
            ColorSlot::Focus,
        );
        let lines = self
            .git_editor_original
            .as_deref()
            .unwrap_or("loading Git original…")
            .lines()
            .collect::<Vec<_>>();
        let visible = box_height.saturating_sub(2) as usize;
        let start = self
            .git_editor_original_scroll
            .min(lines.len().saturating_sub(visible));
        for (index, line) in lines.iter().skip(start).take(visible).enumerate() {
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!("{:>4} {line}", start + index + 1),
                ColorSlot::Default,
            );
        }
    }

    fn draw_git_diff(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(4).clamp(56, 120);
        let box_height = height.saturating_sub(2).max(3);
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        let title = self
            .git_diff_path
            .as_deref()
            .map(|path| {
                format!(
                    " Git diff: {} ({}; c cached, [/] context={}, v split/unified, w whitespace, Tab hunk, x line, s apply, Esc close) ",
                    path.display(),
                    if self.git_diff_staged { "staged" } else { "working tree" },
                    self.git_diff_context
                )
            })
            .unwrap_or_else(|| {
                " Git diff (c cached, [/] context, v split/unified, w whitespace, Tab hunk, x line, s apply, Esc close) "
                    .into()
            });
        frame.write_str(left + 1, top, &title, ColorSlot::Focus);
        let visible = box_height.saturating_sub(2) as usize;
        if self.git_diff_lines.is_empty() {
            frame.write_str(left + 2, top + 1, "(loading or no diff)", ColorSlot::Muted);
            return;
        }
        if self.git_diff_split && width >= 100 {
            if let Some(diff) = self.git_diff_data.as_ref() {
                let language = self
                    .git_diff_path
                    .as_deref()
                    .map(|path| mtide_syntax::detect_with_text(path, ""))
                    .unwrap_or(mtide_syntax::Language::PlainText);
                let rows = render_git_split(diff, &self.git_diff_line_hunks);
                let divider = left + box_width / 2;
                let column_width = box_width.saturating_sub(3) / 2;
                frame.write_str(left + 1, top + 1, "OLD", ColorSlot::Muted);
                frame.write_str(divider + 1, top + 1, "NEW", ColorSlot::Muted);
                for (index, (old, new, kind, hunk)) in rows
                    .iter()
                    .skip(self.git_diff_scroll)
                    .take(visible.saturating_sub(1))
                    .enumerate()
                {
                    let old = old
                        .graphemes(true)
                        .take(column_width as usize)
                        .collect::<String>();
                    let new = new
                        .graphemes(true)
                        .take(column_width as usize)
                        .collect::<String>();
                    write_git_diff_line(
                        frame,
                        left + 1,
                        top + 2 + index as u16,
                        "",
                        &old,
                        *kind,
                        *hunk == Some(self.git_diff_hunk_selected),
                        language,
                    );
                    write_git_diff_line(
                        frame,
                        divider + 1,
                        top + 2 + index as u16,
                        "",
                        &new,
                        *kind,
                        *hunk == Some(self.git_diff_hunk_selected),
                        language,
                    );
                }
                return;
            }
        }
        for (index, (line, kind)) in self
            .git_diff_lines
            .iter()
            .skip(self.git_diff_scroll)
            .take(visible)
            .enumerate()
        {
            let prefix = match kind {
                GitDiffLineKind::Equal => " ",
                GitDiffLineKind::Added => "+",
                GitDiffLineKind::Removed => "-",
            };
            let hunk = self
                .git_diff_line_hunks
                .get(self.git_diff_scroll + index)
                .copied()
                .flatten();
            let selected = self
                .git_diff_selected_lines
                .contains(&(self.git_diff_scroll + index));
            let display_line = if matches!(kind, GitDiffLineKind::Equal)
                && !diff_context_visible(
                    self.git_diff_data.as_ref(),
                    self.git_diff_scroll + index,
                    self.git_diff_context,
                ) {
                "⋯".to_owned()
            } else {
                line.clone()
            };
            let language = self
                .git_diff_path
                .as_deref()
                .map(|path| mtide_syntax::detect_with_text(path, ""))
                .unwrap_or(mtide_syntax::Language::PlainText);
            write_git_diff_line(
                frame,
                left + 1,
                top + 1 + index as u16,
                if selected { ">" } else { prefix },
                &display_line,
                *kind,
                selected || hunk == Some(self.git_diff_hunk_selected),
                language,
            );
        }
    }

    fn draw_git_branch_input(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(48, 88);
        let box_height = 5.min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        let title = if self.git_branch_input_rename {
            " Rename branch (Enter apply, Esc cancel) "
        } else {
            " Create branch (Enter create, Esc cancel) "
        };
        frame.write_str(left + 1, top, title, ColorSlot::Focus);
        frame.write_str(
            left + 2,
            top + 2,
            &format!("> {}", self.git_branch_input),
            ColorSlot::Default,
        );
        frame.write_str(
            left + 2,
            top + 3,
            "Branch names are passed to git unchanged.",
            ColorSlot::Muted,
        );
    }

    fn draw_workspace_root_input(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(52, 104);
        let box_height = 6.min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        let title = if self.workspace_root_input_remove {
            " Remove workspace root (Enter apply, Esc cancel) "
        } else {
            " Add workspace root (Enter apply, Esc cancel) "
        };
        frame.write_str(left + 1, top, title, ColorSlot::Focus);
        frame.write_str(
            left + 2,
            top + 2,
            &format!("> {}", self.workspace_root_input),
            ColorSlot::Default,
        );
        frame.write_str(
            left + 2,
            top + 3,
            "Enter an absolute or workspace-relative directory path.",
            ColorSlot::Muted,
        );
    }

    fn draw_terminal_rename(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(44, 80);
        let box_height = 5.min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            " Rename terminal (Enter apply, Esc cancel) ",
            ColorSlot::Focus,
        );
        frame.write_str(
            left + 2,
            top + 2,
            &format!("> {}", self.terminal_rename_input),
            ColorSlot::Default,
        );
        frame.write_str(
            left + 2,
            top + 3,
            "Names are local to this workspace session.",
            ColorSlot::Muted,
        );
    }

    fn draw_save_as(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(8).clamp(52, 104);
        let box_height = 6.min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            " Save stdin buffer as (Enter save, Esc cancel) ",
            ColorSlot::Focus,
        );
        frame.write_str(
            left + 2,
            top + 2,
            &format!("> {}", self.save_as_input),
            ColorSlot::Default,
        );
        frame.write_str(
            left + 2,
            top + 3,
            "Relative paths use the current working directory.",
            ColorSlot::Muted,
        );
    }

    fn draw_terminal_panel(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let Some(session) = self.terminal_panel.sessions.selected() else {
            return;
        };
        let panel_height = height.min(8);
        let top = height.saturating_sub(panel_height + 1);
        frame.write_str(
            0,
            top,
            &format!(
                " TERMINAL [{}/{}] {}  Ctrl+Shift+N new  Ctrl+Shift+R rename  Ctrl+PageUp/Down switch  Ctrl+Shift+X close",
                self.terminal_panel.sessions.selected_index().map(|index| index + 1).unwrap_or(0),
                self.terminal_panel.sessions.len(),
                session.name()
            ),
            mtide_api::ColorSlot::Focus,
        );
        for row in 0..panel_height.saturating_sub(1) {
            for column in 0..width as usize {
                if let Some(cell) = session.screen.cell(row as usize, column) {
                    frame.set(
                        column as u16,
                        top + 1 + row,
                        Cell {
                            symbol: if cell.continuation {
                                ' '
                            } else {
                                cell.character
                            },
                            style: terminal_color_slot(if cell.style.inverse {
                                cell.style.background
                            } else {
                                cell.style.foreground
                            }),
                            background: terminal_background_slot(if cell.style.inverse {
                                cell.style.foreground
                            } else {
                                cell.style.background
                            }),
                        },
                    );
                }
            }
        }
        let _ = width;
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, width: u16, height: u16) {
        let terminal_top = height.saturating_sub(height.min(8) + 1);
        if self.terminal_panel.sessions.selected().is_some()
            && (self.terminal_panel.is_focused() || mouse.row > terminal_top)
        {
            self.handle_terminal_mouse(mouse, height);
        } else {
            self.handle_editor_mouse(mouse, width, height);
        }
    }

    fn tab_at_column(&self, column: u16) -> Option<usize> {
        let mut offset = 0_u16;
        for (index, tab) in self.tabs.iter().enumerate() {
            let title = tab
                .path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("[untitled]");
            let label = format!(
                "{}{}{}{}{}",
                if index == self.active_tab { "[" } else { " " },
                if self.file_icons {
                    format!("[{}] ", file_icon(tab.path.as_deref()))
                } else {
                    String::new()
                },
                title,
                if tab.dirty { "*" } else { "" },
                if index == self.active_tab { "]" } else { " " },
            );
            let width = label.graphemes(true).count() as u16;
            if column >= offset && column < offset.saturating_add(width) {
                return Some(index);
            }
            offset = offset.saturating_add(width);
        }
        None
    }

    fn editor_mouse_position(
        &self,
        column: u16,
        row: u16,
        width: u16,
        height: u16,
    ) -> Option<usize> {
        let geometry = layout_with_sidebar(width, height, !self.zen_mode, self.sidebar_width);
        let editor_x = geometry.sidebar.map(|(sidebar, _)| sidebar).unwrap_or(0);
        let top: u16 = if self.searching { 2 } else { 1 };
        if row < top || row >= height.saturating_sub(1) || column < editor_x.saturating_add(5) {
            return None;
        }
        let line = (row - top) as usize;
        let text = self.document.text();
        let contents = text.split('\n').collect::<Vec<_>>();
        let line_text = contents.get(line).copied().unwrap_or("");
        let grapheme_column = column
            .saturating_sub(editor_x.saturating_add(5))
            .min(line_text.graphemes(true).count() as u16) as usize;
        self.document.line_column_to_byte(line, grapheme_column)
    }

    fn editor_scrollbar_line(
        &self,
        column: u16,
        row: u16,
        width: u16,
        height: u16,
    ) -> Option<usize> {
        if self.scrollbar_mode == "hidden" {
            return None;
        }
        let geometry = layout_with_sidebar(width, height, !self.zen_mode, self.sidebar_width);
        let editor_x = geometry.sidebar.map(|(sidebar, _)| sidebar).unwrap_or(0);
        let editor_width = geometry.editor.0;
        if editor_width == 0 || column != editor_x.saturating_add(editor_width.saturating_sub(1)) {
            return None;
        }
        let text = self.document.text();
        let lines = text.split('\n').collect::<Vec<_>>();
        let language = self
            .path
            .as_deref()
            .map(|path| mtide_syntax::detect_with_text(path, &text))
            .unwrap_or(mtide_syntax::Language::Rust);
        let top: u16 = if self.searching { 2 } else { 1 };
        let sticky = if self.sticky_context && !self.large_file_mode {
            sticky_context_lines(&text, language, self.current_line(), 2).len()
        } else {
            0
        };
        let content_top = top.saturating_add(sticky as u16);
        let content_height = height.saturating_sub(content_top + 1);
        if content_height == 0 || row < content_top || row >= content_top + content_height {
            return None;
        }
        let relative = (row - content_top) as usize;
        let last_line = lines.len().saturating_sub(1).max(1);
        Some(
            relative.saturating_mul(last_line) / (content_height as usize).saturating_sub(1).max(1),
        )
    }

    fn handle_editor_mouse(&mut self, mouse: MouseEvent, width: u16, height: u16) {
        if mouse.row == 0 && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(index) = self.tab_at_column(mouse.column) {
                if index != self.active_tab {
                    self.sync_current_tab();
                    self.active_tab = index;
                    self.restore_editor_tab(self.tabs[index].clone());
                }
                return;
            }
        }
        let geometry = layout_with_sidebar(width, height, !self.zen_mode, self.sidebar_width);
        if let Some((sidebar_width, _)) = geometry.sidebar {
            let near_divider = mouse.column.abs_diff(sidebar_width) <= 1;
            if near_divider || self.sidebar_dragging {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left) => {
                        self.sidebar_dragging = true;
                        self.sidebar_width = Some(mouse.column.clamp(20, width.saturating_sub(40)));
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        self.sidebar_dragging = false;
                    }
                    _ => {}
                }
                return;
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(line) = self.editor_scrollbar_line(mouse.column, mouse.row, width, height) {
                self.cursor = self
                    .document
                    .line_column_to_byte(line, 0)
                    .unwrap_or(self.cursor);
                self.selection = None;
                self.notification = Some(format!("jumped to line {}", line + 1));
                return;
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(cursor) =
                    self.editor_mouse_position(mouse.column, mouse.row, width, height)
                {
                    self.cursor = cursor;
                    self.selection = None;
                    self.mouse_selection_anchor = Some(cursor);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.mouse_selection_anchor else {
                    return;
                };
                if let Some(cursor) =
                    self.editor_mouse_position(mouse.column, mouse.row, width, height)
                {
                    self.cursor = cursor;
                    self.selection = (anchor != cursor).then(|| Selection::new(anchor, cursor));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.mouse_selection_anchor = None;
            }
            MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                self.horizontal_scroll = self.horizontal_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                self.horizontal_scroll = self.horizontal_scroll.saturating_add(3);
            }
            MouseEventKind::ScrollUp => {
                self.move_vertical(-3);
            }
            MouseEventKind::ScrollDown => {
                self.move_vertical(3);
            }
            _ => {}
        }
    }

    fn handle_terminal_mouse(&mut self, mouse: MouseEvent, height: u16) {
        let Some((screen_height, screen_width, mouse_reporting)) =
            self.terminal_panel.sessions.selected().map(|session| {
                (
                    session.screen.height(),
                    session.screen.width(),
                    session.screen.mouse_reporting(),
                )
            })
        else {
            return;
        };
        let panel_height = height.min(8);
        let top = height.saturating_sub(panel_height + 1);
        if mouse.row <= top || mouse.row >= height.saturating_sub(1) {
            return;
        }
        let row = (mouse.row - top - 1) as usize;
        let column = mouse.column as usize;
        if row >= screen_height || column >= screen_width {
            return;
        }
        if mouse_reporting {
            if let Some(bytes) = terminal_mouse_bytes(mouse.kind, column + 1, row + 1) {
                if let Some(session) = self.terminal_panel.sessions.selected_mut() {
                    let _ = session.send_input(&bytes);
                }
            }
            self.terminal_panel.set_focus(true);
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.terminal_panel.set_focus(true);
                self.terminal_selection = Some(((row, column), (row, column)));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((anchor, active)) = self.terminal_selection.as_mut() {
                    *active = (row, column);
                    let _ = anchor;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {}
            _ => {}
        }
    }

    fn handle_terminal_paste(&mut self, text: &str) -> Result<(), AppError> {
        if !self.terminal_panel.is_focused() {
            return Ok(());
        }
        let bracketed = self
            .terminal_panel
            .sessions
            .selected()
            .is_some_and(|session| session.screen.bracketed_paste());
        let mut bytes = Vec::with_capacity(text.len() + 16);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        if let Some(session) = self.terminal_panel.sessions.selected_mut() {
            session.send_input(&bytes)?;
        }
        Ok(())
    }

    fn copy_terminal_selection(&mut self) {
        let Some((anchor, active)) = self.terminal_selection else {
            self.notification = Some("select terminal output before copying".into());
            return;
        };
        let Some(session) = self.terminal_panel.sessions.selected() else {
            return;
        };
        let (start, end) = if anchor <= active {
            (anchor, active)
        } else {
            (active, anchor)
        };
        let mut output = String::new();
        for row in start.0..=end.0 {
            let line = session.screen.row_text(row);
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 {
                end.1.saturating_add(1)
            } else {
                line.chars().count()
            };
            let selected_line = line
                .chars()
                .skip(from)
                .take(to.saturating_sub(from))
                .collect::<String>();
            output.push_str(selected_line.trim_end_matches(' '));
            if row != end.0 {
                output.push('\n');
            }
        }
        let output = output.trim_end_matches(' ').to_owned();
        if output.is_empty() {
            self.notification = Some("terminal selection is empty".into());
            return;
        }
        self.clipboard = output.clone();
        self.clipboard_history.push(output);
        self.notification = Some("terminal selection copied".into());
    }

    fn command_matches(&self) -> Vec<CommandEntry> {
        self.commands.search(&self.command_query, 12)
    }

    fn open_command_palette(&mut self) {
        self.command_palette = true;
        self.command_query.clear();
        self.command_selected = 0;
    }

    fn execute_command(&mut self, command: &str) -> Result<(), AppError> {
        self.commands.mark_used(command);
        match command {
            "help.keybindings" => {
                self.key_help = true;
                self.key_help_query.clear();
                Ok(())
            }
            "editor.save" => self.save(),
            "editor.saveAs" => {
                self.open_save_as();
                Ok(())
            }
            "editor.nextTab" => {
                self.switch_tab(1);
                Ok(())
            }
            "editor.previousTab" => {
                self.switch_tab(-1);
                Ok(())
            }
            "editor.closeTab" => {
                self.close_tab(false);
                Ok(())
            }
            "editor.pinTab" => {
                self.pin_current_tab();
                Ok(())
            }
            "editor.zen" => {
                self.zen_mode = !self.zen_mode;
                self.notification = Some(if self.zen_mode {
                    "zen mode enabled".into()
                } else {
                    "zen mode disabled".into()
                });
                Ok(())
            }
            "editor.split" => {
                self.editor_split = !self.editor_split;
                self.notification = Some(
                    if self.editor_split {
                        "editor split enabled (Ctrl-\\ to toggle)"
                    } else {
                        "editor split disabled"
                    }
                    .into(),
                );
                Ok(())
            }
            "editor.splitHorizontal" => {
                self.editor_split = true;
                self.editor_split_horizontal = !self.editor_split_horizontal;
                self.notification = Some(
                    if self.editor_split_horizontal {
                        "horizontal editor split enabled"
                    } else {
                        "vertical editor split enabled"
                    }
                    .into(),
                );
                Ok(())
            }
            "editor.stickyContext" => {
                self.sticky_context = !self.sticky_context;
                self.notification = Some(
                    if self.sticky_context {
                        "sticky context enabled"
                    } else {
                        "sticky context disabled"
                    }
                    .into(),
                );
                Ok(())
            }
            "ui.toggleTheme" => self.toggle_theme(),
            "ui.theme.preview" => {
                self.begin_theme_preview();
                Ok(())
            }
            "ui.keymap.vim" => self.set_keymap("vim"),
            "ui.keymap.emacs" => self.set_keymap("emacs"),
            "ui.keymap.vscode" => self.set_keymap("vscode"),
            "ui.keymap.borland" => self.set_keymap("borland"),
            "ui.keymap.importVimrc" => self.import_vimrc_keymap(),
            "navigation.recentFiles" => {
                self.open_recent_files();
                Ok(())
            }
            "navigation.quickOpen" => {
                self.open_file_finder();
                Ok(())
            }
            "search.workspace" => {
                self.open_workspace_search();
                Ok(())
            }
            "selection.expand" => {
                self.expand_selection();
                Ok(())
            }
            "selection.shrink" => {
                self.shrink_selection();
                Ok(())
            }
            "terminal.toggle" => self.toggle_terminal(),
            "terminal.new" => self.new_terminal_session(),
            "terminal.rename" => {
                self.open_terminal_rename();
                Ok(())
            }
            "terminal.next" => {
                self.terminal_panel.sessions.next();
                self.terminal_panel.set_focus(true);
                Ok(())
            }
            "terminal.previous" => {
                self.terminal_panel.sessions.previous();
                self.terminal_panel.set_focus(true);
                Ok(())
            }
            "terminal.close" => {
                self.close_terminal_session();
                Ok(())
            }
            "terminal.link" => self.jump_to_terminal_link(),
            "task.build" => self.start_task(false),
            "task.run" => self.start_task(true),
            "task.test" => self.start_test_task(),
            "task.cancel" => self.cancel_task(),
            "git.cancel" => {
                self.cancel_git_operations();
                Ok(())
            }
            "task.results" => {
                self.task_results_open = true;
                self.task_result_selected = 0;
                Ok(())
            }
            "task.output" => {
                self.task_output_open =
                    self.task_run.is_some() || !self.task_output_history.is_empty();
                self.task_output_scroll = 0;
                self.task_output_tab = 0;
                Ok(())
            }
            "navigation.toggleBookmark" => {
                self.toggle_bookmark();
                Ok(())
            }
            "navigation.clearBookmarks" => {
                self.clear_bookmarks();
                Ok(())
            }
            "git.panel" => {
                self.git_panel_open = true;
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
                Ok(())
            }
            "git.diffEditor" => {
                self.toggle_editor_diff_mode();
                Ok(())
            }
            "git.diffStageHunk" => self.stage_editor_current_hunk(),
            "git.diffRollbackHunk" => self.rollback_editor_current_hunk(),
            "git.diffOriginal" => {
                self.toggle_editor_original();
                Ok(())
            }
            "git.commit" => {
                self.git_panel_open = true;
                self.git_commit_open = true;
                self.git_commit_message.clear();
                self.git_commit_amend = false;
                self.git_commit_feedback.clear();
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
                Ok(())
            }
            "git.conflict.ours" => self.resolve_conflict_at_cursor(ConflictChoice::Ours),
            "git.conflict.theirs" => self.resolve_conflict_at_cursor(ConflictChoice::Theirs),
            "git.conflict.both" => self.resolve_conflict_at_cursor(ConflictChoice::Both),
            "git.conflict.neither" => self.resolve_conflict_at_cursor(ConflictChoice::Neither),
            "git.conflict.allOurs" => self.resolve_all_conflicts(ConflictChoice::Ours),
            "git.conflict.allTheirs" => self.resolve_all_conflicts(ConflictChoice::Theirs),
            "git.conflict.allBoth" => self.resolve_all_conflicts(ConflictChoice::Both),
            "git.conflict.allNeither" => self.resolve_all_conflicts(ConflictChoice::Neither),
            "git.conflict.restore" => self.restore_conflict_state(),
            "workspace.addRoot" => {
                self.open_workspace_root_input(false);
                Ok(())
            }
            "workspace.removeRoot" => {
                self.open_workspace_root_input(true);
                Ok(())
            }
            "workspace.exportDiagnostics" => self.export_diagnostics_bundle(),
            _ => Ok(()),
        }
    }

    fn cancel_git_operations(&mut self) {
        if let Some(cancellation) = &self.git_status_cancel {
            cancellation.cancel();
        }
        if let Some(cancellation) = &self.git_diff_cancel {
            cancellation.cancel();
        }
        if let Some(cancellation) = &self.git_remote_cancel {
            cancellation.cancel();
        }
        if let Some(cancellation) = &self.git_blame_cancel {
            cancellation.cancel();
        }
        if let Some(cancellation) = &self.git_history_detail_cancel {
            cancellation.cancel();
        }
        self.notification = Some("Git operation cancellation requested".into());
    }

    fn apply_keymap_action(&mut self, action: Action) -> Result<bool, AppError> {
        match action {
            Action::Save => {
                self.save()?;
                Ok(false)
            }
            Action::Quit => Ok(true),
            Action::Undo => {
                let _ = self.document.undo();
                Ok(false)
            }
            Action::Redo => {
                let _ = self.document.redo();
                Ok(false)
            }
            Action::QuickOpen => {
                self.open_file_finder();
                Ok(false)
            }
            Action::CommandPalette => {
                self.open_command_palette();
                Ok(false)
            }
            Action::Find => {
                self.searching = true;
                self.search_query.clear();
                self.search_matches.clear();
                Ok(false)
            }
            Action::ToggleTerminal => {
                self.toggle_terminal()?;
                Ok(false)
            }
            Action::Build => {
                self.start_task(false)?;
                Ok(false)
            }
            Action::Run => {
                self.start_task(true)?;
                Ok(false)
            }
            Action::NavigateBack => {
                self.navigate_back();
                Ok(false)
            }
            Action::NavigateForward => {
                self.navigate_forward();
                Ok(false)
            }
            Action::AddCursor => {
                self.add_cursor_below();
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn set_keymap(&mut self, name: &str) -> Result<(), AppError> {
        self.keymap_preset = name.to_owned();
        self.keymap = Keymap::preset(name);
        self.save_workspace_file()?;
        self.notification = Some(format!("keymap switched to {name}"));
        Ok(())
    }

    fn import_vimrc_keymap(&mut self) -> Result<(), AppError> {
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            self.notification = Some("HOME is not available; cannot locate .vimrc".into());
            return Ok(());
        };
        let path = home.join(".vimrc");
        match self.keymap.import_vimrc(&path) {
            Ok((imported, ignored)) => {
                self.keymap_import_path = Some(path);
                self.save_workspace_file()?;
                let conflicts = self.keymap.conflicts().len();
                self.notification = Some(format!(
                    "imported {imported} vimrc mapping(s); ignored {}; conflicts: {conflicts}",
                    ignored.len()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.notification = Some("~/.vimrc was not found".into());
            }
            Err(error) => self.notification = Some(format!("vimrc import failed: {error}")),
        }
        Ok(())
    }

    fn handle_command_palette(&mut self, key: KeyEvent) -> Result<bool, AppError> {
        match key.code {
            KeyCode::Esc => {
                self.command_palette = false;
            }
            KeyCode::Up => {
                self.command_selected = self.command_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let count = self.command_matches().len();
                if count > 0 {
                    self.command_selected = (self.command_selected + 1).min(count - 1);
                }
            }
            KeyCode::Backspace => {
                self.command_query.pop();
                self.command_selected = 0;
            }
            KeyCode::Enter => {
                if let Some(entry) = self.command_matches().get(self.command_selected).cloned() {
                    self.command_palette = false;
                    self.execute_command(&entry.id)?;
                }
            }
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if character == '?' && self.command_query.is_empty() {
                    self.command_palette = false;
                    self.key_help = true;
                    self.key_help_query.clear();
                } else {
                    self.command_query.push(character);
                    self.command_selected = 0;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn draw_command_palette(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let matches = self.command_matches();
        let box_width = width.saturating_sub(8).clamp(24, 72);
        let box_height = (matches.len() as u16 + 3).min(height.saturating_sub(4));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                mtide_api::ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            &format!(" Command: {} ", self.command_query),
            mtide_api::ColorSlot::Focus,
        );
        for (index, entry) in matches
            .iter()
            .enumerate()
            .take(box_height.saturating_sub(2) as usize)
        {
            let marker = if index == self.command_selected {
                ">"
            } else {
                " "
            };
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!(
                    "{marker} {:<36} {}",
                    entry.title,
                    entry.shortcut.clone().unwrap_or_default()
                ),
                if index == self.command_selected {
                    mtide_api::ColorSlot::Focus
                } else {
                    mtide_api::ColorSlot::Default
                },
            );
        }
    }

    fn draw_workspace_search(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let box_width = width.saturating_sub(6).clamp(56, 120);
        let visible = self
            .workspace_search_results
            .len()
            .min(height.saturating_sub(5) as usize);
        let box_height = (visible as u16 + 5).min(height.saturating_sub(2)).max(5);
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            &format!(
                " Workspace {} [{}{}] (Ctrl+R replace, Ctrl+Z undo): {} ",
                if self.workspace_replace_mode {
                    "replace"
                } else {
                    "search"
                },
                if self.workspace_search_regex {
                    "regex"
                } else {
                    "literal"
                },
                if self.workspace_search_case_sensitive {
                    ",case"
                } else {
                    ",nocase"
                },
                if self.workspace_replace_mode {
                    &self.workspace_replace_value
                } else {
                    &self.workspace_search_query
                },
            ),
            ColorSlot::Focus,
        );
        if let Some(error) = &self.workspace_search_error {
            frame.write_str(left + 2, top + 2, error, ColorSlot::Error);
        }
        if self.workspace_search_results.is_empty() {
            frame.write_str(
                left + 2,
                top + if self.workspace_search_error.is_some() {
                    3
                } else {
                    2
                },
                if self.workspace_search_job.is_some() {
                    "searching..."
                } else if self.workspace_search_query.is_empty() {
                    "type a query"
                } else {
                    "no matches"
                },
                ColorSlot::Muted,
            );
        }
        for (index, result) in self
            .workspace_search_results
            .iter()
            .take(visible)
            .enumerate()
        {
            let marker = if index == self.workspace_search_selected {
                ">"
            } else {
                " "
            };
            let line = format!(
                "{marker} {}:{}:{} {}",
                result.path.display(),
                result.line,
                result.column + 1,
                result.text.trim()
            );
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &line,
                if index == self.workspace_search_selected {
                    ColorSlot::Focus
                } else {
                    ColorSlot::Default
                },
            );
        }
        frame.write_str(
            left + 1,
            top + box_height.saturating_sub(2),
            &format!("{} matches", self.workspace_search_results.len()),
            ColorSlot::Muted,
        );
    }

    fn draw_key_help(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let entries = self.commands.search(&self.key_help_query, usize::MAX);
        let box_width = width.saturating_sub(8).clamp(32, 82);
        let box_height = (entries.len() as u16 + 3).min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                mtide_api::ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            &format!(
                " Keyboard shortcuts: {} (Esc to close) ",
                self.key_help_query
            ),
            mtide_api::ColorSlot::Focus,
        );
        for (index, entry) in entries
            .iter()
            .enumerate()
            .take(box_height.saturating_sub(2) as usize)
        {
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!(
                    " {:<36} {}",
                    entry.title,
                    entry.shortcut.clone().unwrap_or_default()
                ),
                mtide_api::ColorSlot::Default,
            );
        }
    }

    fn draw_task_panel(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let Some(task) = self.task_run.as_ref() else {
            return;
        };
        let panel_height = height.min(6);
        let top = height.saturating_sub(panel_height + 1);
        let state = match task.state() {
            TaskState::Running => "running",
            TaskState::Finished(status) if status.0 == Some(0) => "passed",
            TaskState::Finished(_) => "failed",
            TaskState::Cancelled if task.timed_out() => "timed out",
            TaskState::Cancelled => "cancelled",
        };
        frame.write_str(
            0,
            top,
            &format!(
                " TASK {} [{state}]  F9 build  Ctrl+F9 run  Ctrl+Shift+T test  Ctrl+Shift+F9 cancel  F4 problem ",
                task.label()
            ),
            mtide_api::ColorSlot::Focus,
        );
        let lines = task
            .output()
            .lines()
            .rev()
            .take(panel_height.saturating_sub(1) as usize);
        for (offset, line) in lines.collect::<Vec<_>>().into_iter().rev().enumerate() {
            frame.write_str(
                0,
                top + 1 + offset as u16,
                line,
                mtide_api::ColorSlot::Default,
            );
        }
        let _ = width;
    }

    fn draw_task_results(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let Some(task) = self.task_run.as_ref() else {
            return;
        };
        let tree = task.result_tree();
        let mut lines = tree
            .children
            .iter()
            .map(|node| {
                let marker = match node.status {
                    ResultStatus::Passed => "✓",
                    ResultStatus::Failed => "✗",
                    ResultStatus::Skipped => "·",
                    ResultStatus::Running => "…",
                };
                format!("{marker} {}", node.name)
            })
            .collect::<Vec<_>>();
        lines.extend(task.problems().iter().map(|problem| {
            format!(
                "! {}:{}{} {}",
                problem.file.display(),
                problem.line,
                problem
                    .column
                    .map(|column| format!(":{column}"))
                    .unwrap_or_default(),
                problem.message
            )
        }));
        let box_width = width.saturating_sub(8).clamp(48, 100);
        let visible = lines.len().min(height.saturating_sub(4) as usize);
        let box_height = (visible as u16 + 3).max(3).min(height.saturating_sub(2));
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        frame.write_str(
            left + 1,
            top,
            " Task results / problems (F4 jump, Esc close) ",
            ColorSlot::Focus,
        );
        for (index, line) in lines.iter().take(visible).enumerate() {
            let selected = index == self.task_result_selected;
            frame.write_str(
                left + 1,
                top + 1 + index as u16,
                &format!("{} {line}", if selected { ">" } else { " " }),
                if selected {
                    ColorSlot::Focus
                } else {
                    ColorSlot::Default
                },
            );
        }
    }

    fn draw_task_output(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let Some((label, output, _)) = self.selected_task_output() else {
            return;
        };
        let box_width = width.saturating_sub(4).clamp(56, 120);
        let box_height = height.saturating_sub(2).max(5);
        let left = width.saturating_sub(box_width) / 2;
        let top = height.saturating_sub(box_height) / 2;
        for row in 0..box_height {
            frame.write_str(
                left,
                top + row,
                &" ".repeat(box_width as usize),
                ColorSlot::Muted,
            );
        }
        let search = self.task_output_search.as_deref().unwrap_or("");
        frame.write_str(
            left + 1,
            top,
            &format!(
                " Task output: {}  tab {}/{}  Tab switch  /{} search  y copy  l link  Esc close ",
                label,
                self.task_output_tab,
                self.task_output_history.len(),
                search
            ),
            ColorSlot::Focus,
        );
        let lines = output.lines().collect::<Vec<_>>();
        let visible = box_height.saturating_sub(2) as usize;
        let start = self
            .task_output_scroll
            .min(lines.len().saturating_sub(visible));
        for (index, line) in lines.iter().skip(start).take(visible).enumerate() {
            let color = if !search.is_empty() && line.contains(search) {
                ColorSlot::Focus
            } else {
                ColorSlot::Default
            };
            if search.is_empty() {
                frame.write_ansi(left + 1, top + 1 + index as u16, line, ColorSlot::Default);
            } else {
                frame.write_str(left + 1, top + 1 + index as u16, line, color);
            }
        }
        let _ = width;
    }

    fn poll_task(&mut self) -> Result<(), AppError> {
        self.poll_background_tasks()?;
        let was_running = self
            .task_run
            .as_ref()
            .is_some_and(|task| matches!(task.state(), TaskState::Running));
        let mut finished = false;
        if let Some(task) = self.task_run.as_mut() {
            task.poll()?;
            finished = was_running && !matches!(task.state(), TaskState::Running);
        }
        if finished {
            self.remember_finished_task();
            self.task_output_tab = 0;
            let elapsed = self.task_started.take().map(|started| started.elapsed());
            let timed_out = self.task_run.as_ref().is_some_and(TaskRun::timed_out);
            let status = self.task_run.as_ref().and_then(|task| match task.state() {
                TaskState::Finished(status) => Some(status.0 == Some(0)),
                _ => None,
            });
            if status == Some(true) {
                if let Some(next) = self.task_queue.first().cloned() {
                    self.task_queue.remove(0);
                    let label = next.label.clone();
                    self.launch_task_spec(next)?;
                    self.notification = Some(format!("task started: {label}"));
                    return Ok(());
                }
            } else {
                self.task_queue.clear();
            }
            match status {
                Some(true) => {
                    self.task_output_open = false;
                    self.task_results_open = false;
                }
                Some(false) => {
                    self.task_output_open = true;
                    self.task_output_scroll = 0;
                    self.task_output_tab = 0;
                    if self
                        .task_run
                        .as_ref()
                        .is_some_and(|task| !task.problems().is_empty())
                    {
                        let _ = self.jump_to_first_problem();
                    }
                }
                _ => {}
            }
            self.notification = Some(if timed_out {
                "task timed out (300s limit)".into()
            } else {
                match (status, elapsed) {
                    (Some(true), Some(duration)) => {
                        format!("build succeeded ({:.2}s)", duration.as_secs_f64())
                    }
                    (Some(false), Some(duration)) => {
                        format!("build failed ({:.2}s)", duration.as_secs_f64())
                    }
                    _ => "task cancelled".into(),
                }
            });
        }
        Ok(())
    }

    fn poll_git(&mut self) -> Result<(), AppError> {
        if let Some(job) = self.git_history_detail_job.as_ref() {
            if let Ok((root, result)) = job.try_recv() {
                self.git_history_detail_job = None;
                self.git_history_detail_cancel = None;
                if self
                    .git_repository
                    .as_ref()
                    .is_some_and(|repository| repository.root() == root)
                {
                    self.git_history_detail = Some(match result {
                        Ok(detail) => detail,
                        Err(error) => format!("unable to load commit detail: {error}"),
                    });
                }
            }
        }
        if let Some(job) = self.git_remote_job.as_ref() {
            if let Ok((root, operation, result)) = job.try_recv() {
                self.git_remote_job = None;
                self.git_remote_cancel = None;
                if self
                    .git_repository
                    .as_ref()
                    .is_some_and(|repository| repository.root() == root)
                {
                    self.notification = Some(match result {
                        Ok(output) if output.is_empty() => format!("{operation} completed"),
                        Ok(output) => format!("{operation}: {output}"),
                        Err(error) => format!("{operation} failed: {error}"),
                    });
                    self.last_git_poll = Instant::now() - Duration::from_secs(1);
                }
            }
        }
        let completed_blame = self
            .git_blame_job
            .as_ref()
            .and_then(|job| job.try_recv().ok());
        if let Some((path, line, blame)) = completed_blame {
            self.git_blame_job = None;
            self.git_blame_cancel = None;
            if self.path.as_deref() == Some(path.as_path())
                && self.document.byte_to_line(self.cursor).unwrap_or(0) + 1 == line
            {
                self.git_blame = blame;
            }
        }
        if self.git_blame_job.is_none() {
            if let (Some(repository), Some(path)) = (self.git_repository.clone(), self.path.clone())
            {
                let line = self.document.byte_to_line(self.cursor).unwrap_or(0) + 1;
                let request = (path.clone(), line);
                if self.git_blame_request.as_ref() != Some(&request) {
                    let (sender, receiver) = mpsc::channel();
                    let request_path = path.clone();
                    let cancellation = GitCancellation::default();
                    let worker_cancellation = cancellation.clone();
                    std::thread::spawn(move || {
                        let blame = repository
                            .blame_line_cancellable(&request_path, line, &worker_cancellation)
                            .ok()
                            .flatten();
                        let _ = sender.send((request_path, line, blame));
                    });
                    self.git_blame_request = Some(request);
                    self.git_blame_job = Some(receiver);
                    self.git_blame_cancel = Some(cancellation);
                    self.git_blame = None;
                }
            } else {
                self.git_blame_request = None;
                self.git_blame = None;
            }
        }
        if let Some(job) = self.git_diff_job.as_ref() {
            if let Ok((root, lines, original)) = job.try_recv() {
                self.git_diff_job = None;
                self.git_diff_cancel = None;
                if self
                    .git_repository
                    .as_ref()
                    .is_some_and(|repository| repository.root() == root)
                {
                    if let Some(diff) = lines {
                        let (lines, line_hunks, hunks) = render_git_diff(&diff);
                        if self.git_editor_diff_mode {
                            self.git_editor_line_marks = editor_diff_line_marks(&diff);
                            self.git_editor_original = original;
                            self.git_editor_original_scroll = 0;
                        }
                        self.git_diff_data = Some(diff);
                        self.git_diff_lines = lines;
                        self.git_diff_line_hunks = line_hunks;
                        self.git_diff_hunks = hunks;
                        self.git_diff_hunk_selected = 0;
                    } else {
                        self.git_diff_lines.clear();
                        self.notification = Some("unable to load Git diff".into());
                    }
                }
            }
        }
        if self.git_editor_diff_mode && self.git_diff_job.is_none() {
            let current = self.document.text();
            if self.git_editor_diff_snapshot.as_deref() != Some(current.as_str()) {
                self.request_editor_diff();
            }
        }
        let completed = self
            .git_status_job
            .as_ref()
            .and_then(|job| job.try_recv().ok());
        if let Some((root, result)) = completed {
            self.git_status_job = None;
            self.git_status_cancel = None;
            if self
                .git_repository
                .as_ref()
                .is_some_and(|repository| repository.root() == root)
            {
                if let Some((
                    branch,
                    counts,
                    entries,
                    branches,
                    history,
                    stashes,
                    history_snapshot,
                    history_offset,
                    history_failed,
                )) = result
                {
                    self.git_branch = Some(branch);
                    self.git_counts = counts;
                    self.git_entries = entries;
                    self.git_branches = branches;
                    if history_offset > 0 {
                        if history_failed {
                            self.notification = Some(
                                "Git history changed; refresh before loading more commits".into(),
                            );
                        } else if self.git_history_snapshot == history_snapshot {
                            self.git_history.extend(history);
                        } else {
                            self.git_history.clear();
                            self.git_history_offset = 0;
                            self.notification = Some(
                                "Git history changed; refresh before loading more commits".into(),
                            );
                        }
                    } else {
                        self.git_history = history;
                        self.git_history_offset = 0;
                    }
                    if !history_failed {
                        self.git_history_snapshot = history_snapshot;
                    }
                    self.git_history_requested = false;
                    self.git_stashes = stashes;
                    self.git_selected = self
                        .git_selected
                        .min(self.git_entries.len().saturating_sub(1));
                    self.git_branch_selected = self
                        .git_branch_selected
                        .min(self.git_branches.len().saturating_sub(1));
                    self.git_history_selected = self
                        .git_history_selected
                        .min(self.git_history.len().saturating_sub(1));
                    self.git_stash_selected = self
                        .git_stash_selected
                        .min(self.git_stashes.len().saturating_sub(1));
                }
            }
        }
        if self.git_status_job.is_some() {
            return Ok(());
        }
        if self.last_git_poll.elapsed() < Duration::from_millis(500) {
            return Ok(());
        }
        self.last_git_poll = Instant::now();
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        let root = repository.root().to_path_buf();
        let history_offset = self.git_history_offset;
        let history_snapshot = self.git_history_snapshot.clone();
        let history_filter = self.git_history_filter.clone();
        let history_filter_kind = self.git_history_filter_kind;
        let history_file_only = self.git_history_file_only;
        let history_path = self
            .path
            .as_deref()
            .and_then(|path| path.strip_prefix(repository.root()).ok())
            .map(Path::to_path_buf);
        let load_history = self.git_history_requested || self.git_history.is_empty();
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let history_snapshot = history_snapshot.or_else(|| repository.head_id().ok());
            let result = repository
                .status_with_cancel(&worker_cancellation)
                .ok()
                .map(|entries| {
                    let mut staged = 0;
                    let mut unstaged = 0;
                    let mut untracked = 0;
                    let mut conflicts = 0;
                    for entry in &entries {
                        if entry.index_code != '.' && entry.index_code != ' ' {
                            staged += 1;
                        }
                        if entry.worktree_code != '.' && entry.worktree_code != ' ' {
                            unstaged += 1;
                        }
                        if entry.index_code == '?' {
                            untracked += 1;
                        }
                        if entry.index_code == 'U' || entry.worktree_code == 'U' {
                            conflicts += 1;
                        }
                    }
                    let mut entries = entries;
                    entries.sort_by_key(|entry| {
                        let group = if entry.index_code == 'U' || entry.worktree_code == 'U' {
                            0
                        } else if entry.index_code != '.' && entry.index_code != ' ' {
                            1
                        } else if entry.index_code == '?' {
                            3
                        } else {
                            2
                        };
                        (group, entry.path.clone())
                    });
                    let branches = repository
                        .branches_cancellable(&worker_cancellation)
                        .unwrap_or_default();
                    let (history, history_failed) = if load_history {
                        match history_snapshot.as_deref() {
                            Some(snapshot) => {
                                let (author, keyword) = match history_filter_kind {
                                    GitHistoryFilterKind::Keyword => {
                                        (None, Some(history_filter.as_str()))
                                    }
                                    GitHistoryFilterKind::Author => {
                                        (Some(history_filter.as_str()), None)
                                    }
                                };
                                match repository.history_page_filtered_cancellable(
                                    snapshot,
                                    history_offset,
                                    10,
                                    if history_file_only {
                                        history_path.as_deref()
                                    } else {
                                        None
                                    },
                                    author,
                                    keyword,
                                    &worker_cancellation,
                                ) {
                                    Ok(history) => (history, false),
                                    Err(_) => (Vec::new(), history_offset > 0),
                                }
                            }
                            None => (Vec::new(), false),
                        }
                    } else {
                        (Vec::new(), false)
                    };
                    let stashes = repository
                        .stashes_cancellable(&worker_cancellation)
                        .unwrap_or_default();
                    let branch = branches
                        .iter()
                        .find(|branch| branch.current)
                        .map(|branch| branch.name.clone())
                        .unwrap_or_else(|| "(detached)".into());
                    (
                        branch,
                        (staged, unstaged, untracked, conflicts),
                        entries,
                        branches,
                        history,
                        stashes,
                        history_snapshot,
                        history_offset,
                        history_failed,
                    )
                });
            let _ = sender.send((root, result));
        });
        self.git_status_job = Some(receiver);
        self.git_status_cancel = Some(cancellation);
        Ok(())
    }

    fn open_git_diff(&mut self) {
        let Some(entry) = self.git_entries.get(self.git_selected).cloned() else {
            return;
        };
        let Some(repository) = self.git_repository.clone() else {
            return;
        };
        let root = repository.root().to_path_buf();
        let path = root.join(&entry.path);
        let ignore_whitespace = self.git_diff_ignore_whitespace;
        let staged = self.git_diff_staged;
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn({
            let path = path.clone();
            move || {
                let diff = if staged {
                    repository
                        .cached_diff_file_with_options_cancellable(
                            &path,
                            ignore_whitespace,
                            &worker_cancellation,
                        )
                        .ok()
                } else {
                    repository
                        .diff_file_with_options_cancellable(
                            &path,
                            ignore_whitespace,
                            &worker_cancellation,
                        )
                        .ok()
                };
                let _ = sender.send((root, diff, None));
            }
        });
        self.git_diff_path = Some(entry.path);
        self.git_diff_lines.clear();
        self.git_diff_line_hunks.clear();
        self.git_diff_hunks.clear();
        self.git_diff_selected_lines.clear();
        self.git_diff_data = None;
        self.git_diff_scroll = 0;
        self.git_diff_open = true;
        self.git_diff_job = Some(receiver);
        self.git_diff_cancel = Some(cancellation);
    }

    fn request_editor_diff(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(repository) = self.git_repository.clone() else {
            return;
        };
        let text = self.document.text();
        let root = repository.root().to_path_buf();
        let repository_root = repository.root().to_path_buf();
        let ignore_whitespace = self.git_diff_ignore_whitespace;
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_path = path.clone();
        let worker_text = text.clone();
        std::thread::spawn(move || {
            let original = repository
                .head_file_text_cancellable(&worker_path, &worker_cancellation)
                .ok();
            let diff = original.as_deref().map(|old| {
                mtide_diff::diff_lines_with_options(old, &worker_text, ignore_whitespace)
            });
            let _ = sender.send((root, diff, original));
        });
        self.git_diff_path = path
            .strip_prefix(&repository_root)
            .ok()
            .map(Path::to_path_buf);
        self.git_editor_diff_snapshot = Some(text);
        self.git_diff_data = None;
        self.git_diff_lines.clear();
        self.git_diff_line_hunks.clear();
        self.git_diff_hunks.clear();
        self.git_diff_job = Some(receiver);
        self.git_diff_cancel = Some(cancellation);
    }

    fn toggle_editor_diff_mode(&mut self) {
        if self.git_editor_diff_mode {
            if let Some(cancellation) = &self.git_diff_cancel {
                cancellation.cancel();
            }
            self.git_editor_diff_mode = false;
            self.git_editor_diff_snapshot = None;
            self.git_editor_original = None;
            self.git_editor_original_open = false;
            self.git_editor_line_marks.clear();
            self.notification = Some("editor diff mode disabled".into());
            return;
        }
        if self.path.is_none() || self.git_repository.is_none() {
            self.notification = Some("open a tracked Git file for editor diff mode".into());
            return;
        }
        self.git_editor_diff_mode = true;
        self.git_editor_diff_snapshot = None;
        self.request_editor_diff();
        self.notification = Some("editor diff mode enabled; edits update decorations".into());
    }

    fn stage_editor_current_hunk(&mut self) -> Result<(), AppError> {
        if !self.git_editor_diff_mode {
            self.notification = Some("enable editor diff mode first".into());
            return Ok(());
        }
        let Some(diff) = self.git_diff_data.clone() else {
            self.notification = Some("editor diff is still loading".into());
            return Ok(());
        };
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let Some(hunk) = diff
            .hunks
            .iter()
            .find(|hunk| hunk.new_range.contains(&line))
            .map(|hunk| hunk.id)
        else {
            self.notification = Some("current line is outside a Git hunk".into());
            return Ok(());
        };
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        if self.document.is_dirty() {
            self.save()?;
        }
        match repository.stage_hunks(&path, &diff, &[hunk]) {
            Ok(()) => {
                self.notification = Some(format!("staged editor diff hunk {}", hunk + 1));
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("editor hunk staging failed: {error}")),
        }
        Ok(())
    }

    fn rollback_editor_current_hunk(&mut self) -> Result<(), AppError> {
        if !self.git_editor_diff_mode {
            self.notification = Some("enable editor diff mode first".into());
            return Ok(());
        }
        let Some(diff) = self.git_diff_data.clone() else {
            self.notification = Some("editor diff is still loading".into());
            return Ok(());
        };
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let Some(hunk) = diff
            .hunks
            .iter()
            .find(|hunk| hunk.new_range.contains(&line))
        else {
            self.notification = Some("current line is outside a Git hunk".into());
            return Ok(());
        };
        let Some(original) = self.git_editor_original.as_deref() else {
            self.notification = Some("Git original is unavailable".into());
            return Ok(());
        };
        let accepted = diff
            .hunks
            .iter()
            .filter(|candidate| candidate.id != hunk.id)
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>();
        let replacement = match apply_hunks(original, &diff, &accepted) {
            Ok(replacement) => replacement,
            Err(error) => {
                self.notification = Some(format!("unable to rollback hunk: {error}"));
                return Ok(());
            }
        };
        let old_len = self.document.text().len();
        self.document.apply(&[Edit {
            range: 0..old_len,
            insert: replacement,
        }])?;
        self.cursor = self.cursor.min(self.document.text().len());
        self.git_editor_diff_snapshot = None;
        self.notification = Some(format!("rolled back editor diff hunk {}", hunk.id + 1));
        Ok(())
    }

    fn toggle_editor_original(&mut self) {
        if self.git_editor_original.is_none() {
            self.notification = Some("Git original is unavailable".into());
            return;
        }
        self.git_editor_original_open = !self.git_editor_original_open;
        self.git_editor_original_scroll = 0;
    }

    fn stage_selected_git_hunk(&mut self) -> Result<(), AppError> {
        let Some(diff) = self.git_diff_data.as_ref() else {
            self.notification = Some("diff is still loading".into());
            return Ok(());
        };
        let Some(path) = self.git_diff_path.clone() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        let Some(&hunk) = self.git_diff_hunks.get(self.git_diff_hunk_selected) else {
            return Ok(());
        };
        match repository.stage_hunks(&repository.root().join(path), diff, &[hunk]) {
            Ok(()) => {
                self.notification = Some(format!("staged diff hunk {}", hunk + 1));
                self.git_diff_open = false;
                self.git_diff_data = None;
                self.git_diff_lines.clear();
                self.git_diff_line_hunks.clear();
                self.git_diff_hunks.clear();
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("hunk staging failed: {error}")),
        }
        Ok(())
    }

    fn unstage_selected_git_hunk(&mut self) -> Result<(), AppError> {
        let Some(diff) = self.git_diff_data.as_ref() else {
            self.notification = Some("diff is still loading".into());
            return Ok(());
        };
        let Some(path) = self.git_diff_path.clone() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        let Some(&hunk) = self.git_diff_hunks.get(self.git_diff_hunk_selected) else {
            return Ok(());
        };
        match repository.unstage_hunks(&repository.root().join(path), diff, &[hunk]) {
            Ok(()) => {
                self.notification = Some(format!("unstaged diff hunk {}", hunk + 1));
                self.git_diff_data = None;
                self.git_diff_lines.clear();
                self.git_diff_line_hunks.clear();
                self.git_diff_hunks.clear();
                self.git_diff_selected_lines.clear();
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
                self.open_git_diff();
            }
            Err(error) => self.notification = Some(format!("hunk unstage failed: {error}")),
        }
        Ok(())
    }

    fn stage_selected_git_lines(&mut self) -> Result<(), AppError> {
        let Some(diff) = self.git_diff_data.as_ref() else {
            self.notification = Some("diff is still loading".into());
            return Ok(());
        };
        let Some(path) = self.git_diff_path.clone() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        if self.git_diff_selected_lines.is_empty() {
            self.notification = Some("select at least one changed line with x".into());
            return Ok(());
        }
        let selected = self
            .git_diff_selected_lines
            .iter()
            .copied()
            .collect::<Vec<_>>();
        match repository.stage_lines(&repository.root().join(path), diff, &selected) {
            Ok(()) => {
                self.notification = Some(format!("staged {} selected line(s)", selected.len()));
                self.git_diff_open = false;
                self.git_diff_data = None;
                self.git_diff_lines.clear();
                self.git_diff_line_hunks.clear();
                self.git_diff_hunks.clear();
                self.git_diff_selected_lines.clear();
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("line staging failed: {error}")),
        }
        Ok(())
    }

    fn unstage_selected_git_lines(&mut self) -> Result<(), AppError> {
        let Some(diff) = self.git_diff_data.as_ref() else {
            self.notification = Some("diff is still loading".into());
            return Ok(());
        };
        let Some(path) = self.git_diff_path.clone() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        let selected = self
            .git_diff_selected_lines
            .iter()
            .copied()
            .collect::<Vec<_>>();
        match repository.unstage_lines(&repository.root().join(path), diff, &selected) {
            Ok(()) => {
                self.notification = Some(format!("unstaged {} selected line(s)", selected.len()));
                self.git_diff_data = None;
                self.git_diff_lines.clear();
                self.git_diff_line_hunks.clear();
                self.git_diff_hunks.clear();
                self.git_diff_selected_lines.clear();
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
                self.open_git_diff();
            }
            Err(error) => self.notification = Some(format!("line unstage failed: {error}")),
        }
        Ok(())
    }

    fn apply_git_branch_input(&mut self) -> Result<(), AppError> {
        let name = self.git_branch_input.trim().to_owned();
        if name.is_empty() {
            self.notification = Some("branch name must not be empty".into());
            return Ok(());
        }
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return Ok(());
        };
        let result = if self.git_branch_input_rename {
            let Some(branch) = self.git_branches.get(self.git_branch_selected) else {
                return Ok(());
            };
            repository.rename_branch(&branch.name, &name)
        } else {
            repository.create_branch(&name, None)
        };
        match result {
            Ok(()) => {
                self.notification = Some(if self.git_branch_input_rename {
                    format!("renamed branch to {name}")
                } else {
                    format!("created branch {name}")
                });
                self.git_branch_input_open = false;
                self.git_branch_input.clear();
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("branch operation failed: {error}")),
        }
        Ok(())
    }

    fn delete_git_branch(&mut self, name: &str) -> Result<(), AppError> {
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        match repository.delete_branch(name, false) {
            Ok(()) => {
                self.notification = Some(format!("deleted branch {name}"));
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => {
                self.notification = Some(format!("branch deletion failed: {error}"));
            }
        }
        Ok(())
    }

    fn start_git_remote(&mut self, action: GitRemoteAction) {
        if self.git_remote_job.is_some() {
            self.notification = Some("another Git remote operation is already running".into());
            return;
        }
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return;
        };
        let root = repository.root().to_path_buf();
        let operation = match action {
            GitRemoteAction::Fetch => "fetch",
            GitRemoteAction::Pull => "pull",
            GitRemoteAction::Push => "push",
            GitRemoteAction::Sync => "sync",
        };
        let (sender, receiver) = mpsc::channel();
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        std::thread::spawn(move || {
            let result = match action {
                GitRemoteAction::Fetch => repository.fetch_cancellable(&worker_cancellation),
                GitRemoteAction::Pull => repository.pull_cancellable(false, &worker_cancellation),
                GitRemoteAction::Push => repository.push_cancellable(&worker_cancellation),
                GitRemoteAction::Sync => repository.sync_cancellable(false, &worker_cancellation),
            };
            let _ = sender.send((root, operation, result));
        });
        self.git_remote_job = Some(receiver);
        self.git_remote_cancel = Some(cancellation);
        self.notification = Some(format!("{operation} started"));
    }

    fn start_git_branch_action(&mut self, action: GitBranchAction, branch: String) {
        if self.git_remote_job.is_some() {
            self.notification = Some("another Git operation is already running".into());
            return;
        }
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return;
        };
        let operation = match action {
            GitBranchAction::Merge => "merge",
            GitBranchAction::Rebase => "rebase",
        };
        let (sender, receiver) = mpsc::channel();
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        let branch_for_job = branch.clone();
        std::thread::spawn(move || {
            let result = match action {
                GitBranchAction::Merge => {
                    repository.merge_cancellable(&branch_for_job, &worker_cancellation)
                }
                GitBranchAction::Rebase => {
                    repository.rebase_cancellable(&branch_for_job, &worker_cancellation)
                }
            }
            .map(|()| String::new());
            let _ = sender.send((repository.root().to_path_buf(), operation, result));
        });
        self.git_remote_job = Some(receiver);
        self.git_remote_cancel = Some(cancellation);
        self.notification = Some(format!("{operation} {branch} started"));
    }

    fn open_git_history_detail(&mut self) {
        if self.git_history_detail_job.is_some() {
            return;
        }
        let Some(commit) = self.git_history.get(self.git_history_selected).cloned() else {
            self.notification = Some("no commit selected".into());
            return;
        };
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return;
        };
        let root = repository.root().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        std::thread::spawn(move || {
            let result = repository.commit_detail_cancellable(&commit.id, &worker_cancellation);
            let _ = sender.send((root, result));
        });
        self.git_history_detail_open = true;
        self.git_history_detail = None;
        self.git_history_detail_scroll = 0;
        self.git_history_detail_job = Some(receiver);
        self.git_history_detail_cancel = Some(cancellation);
    }

    fn refresh_git_history(&mut self) {
        self.git_history.clear();
        self.git_history_selected = 0;
        self.git_history_offset = 0;
        self.git_history_snapshot = None;
        self.git_history_requested = true;
        self.last_git_poll = Instant::now() - Duration::from_secs(1);
    }

    fn open_git_stash_detail(&mut self) {
        if self.git_history_detail_job.is_some() {
            return;
        }
        let Some(stash) = self.git_stashes.get(self.git_stash_selected).cloned() else {
            self.notification = Some("no stash selected".into());
            return;
        };
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return;
        };
        let root = repository.root().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let cancellation = GitCancellation::default();
        let worker_cancellation = cancellation.clone();
        std::thread::spawn(move || {
            let result =
                repository.stash_detail_cancellable(&stash.reference, &worker_cancellation);
            let _ = sender.send((root, result));
        });
        self.git_history_detail_open = true;
        self.git_history_detail = None;
        self.git_history_detail_scroll = 0;
        self.git_history_detail_job = Some(receiver);
        self.git_history_detail_cancel = Some(cancellation);
    }

    fn poll_autosave(&mut self) {
        if self.autosave_interval.is_zero()
            || !self.document.is_dirty()
            || self.last_autosave.elapsed() < self.autosave_interval
            || self.external_change
        {
            return;
        }
        self.last_autosave = Instant::now();
        let result = if self.path.is_some() {
            self.save()
        } else {
            self.persist_session()
        };
        match result {
            Ok(()) => {
                self.notification = Some(if self.path.is_some() {
                    "autosaved".into()
                } else {
                    "autosaved unnamed buffer to session".into()
                });
            }
            Err(AppError::ExternalChange(_)) => {
                self.external_change = true;
                self.notification = Some("autosave paused: external change detected".into());
            }
            Err(error) => {
                self.notification = Some(format!("autosave failed: {error}"));
            }
        }
    }

    fn start_task(&mut self, run: bool) -> Result<(), AppError> {
        let path = self.path.clone();
        self.start_task_named(if run { "run" } else { "build" }, || {
            default_task(path.as_deref(), run)
        })
    }

    fn start_test_task(&mut self) -> Result<(), AppError> {
        let path = self.path.clone();
        self.start_task_named("test", || default_test_task(path.as_deref()))
    }

    fn start_task_named<F>(&mut self, name: &str, fallback: F) -> Result<(), AppError>
    where
        F: FnOnce() -> Option<TaskSpec>,
    {
        if let Some(task) = self
            .task_run
            .as_ref()
            .filter(|task| matches!(task.state(), TaskState::Running))
        {
            if task.label() == name || !self.task_queue.is_empty() {
                self.notification = Some("task is already running".into());
                return Ok(());
            }
            let Some(current) = self.task_run.take() else {
                return Ok(());
            };
            let started = self.task_started.take().unwrap_or_else(Instant::now);
            self.background_tasks.push((current, started));
        }
        let mut specs = self
            .path
            .as_deref()
            .and_then(Path::parent)
            .and_then(find_build_root)
            .and_then(|root| configured_task_plan(&root, name))
            .unwrap_or_else(|| fallback().into_iter().collect());
        let Some(spec) = specs.first().cloned() else {
            self.notification = Some("no build recipe detected".into());
            return Ok(());
        };
        specs.remove(0);
        self.task_queue = specs;
        self.launch_task_spec(spec)?;
        self.notification = Some(format!("task started: {name}"));
        Ok(())
    }

    fn launch_task_spec(&mut self, mut spec: TaskSpec) -> Result<(), AppError> {
        let text = self.document.text();
        let selected_text = self
            .selection
            .and_then(|selection| text.get(selection.range()))
            .unwrap_or_default()
            .to_owned();
        let mut environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
        environment.extend(spec.environment.clone());
        let context = TaskContext {
            workspace_folder: spec.cwd.clone(),
            file: self.path.clone(),
            selected_text,
            line: Some(self.current_line() + 1),
            environment,
        };
        spec = spec.expand(&context);
        self.task_run = Some(TaskRun::start(spec)?);
        self.task_started = Some(Instant::now());
        Ok(())
    }

    fn cancel_task(&mut self) -> Result<(), AppError> {
        let Some(task) = self.task_run.as_mut() else {
            self.notification = Some("no task is running".into());
            return Ok(());
        };
        if !matches!(task.state(), TaskState::Running) {
            self.notification = Some("no task is running".into());
            return Ok(());
        }
        task.cancel()?;
        self.task_queue.clear();
        self.task_started = None;
        self.notification = Some("task cancelled".into());
        Ok(())
    }

    fn remember_finished_task(&mut self) {
        let Some(task) = self.task_run.as_ref() else {
            return;
        };
        let snapshot = TaskOutputSnapshot {
            label: task.label().to_owned(),
            output: task.output().to_owned(),
            cwd: task.cwd().map(Path::to_path_buf),
        };
        self.remember_task_snapshot(snapshot);
    }

    fn remember_task_output(&mut self, task: &TaskRun) {
        self.remember_task_snapshot(TaskOutputSnapshot {
            label: task.label().to_owned(),
            output: task.output().to_owned(),
            cwd: task.cwd().map(Path::to_path_buf),
        });
    }

    fn remember_task_snapshot(&mut self, snapshot: TaskOutputSnapshot) {
        self.task_output_history.push(snapshot);
        const MAX_TASK_OUTPUTS: usize = 8;
        if self.task_output_history.len() > MAX_TASK_OUTPUTS {
            let excess = self.task_output_history.len() - MAX_TASK_OUTPUTS;
            self.task_output_history.drain(..excess);
        }
    }

    fn poll_background_tasks(&mut self) -> Result<(), AppError> {
        let mut finished = Vec::new();
        for (index, (task, _started)) in self.background_tasks.iter_mut().enumerate() {
            let was_running = matches!(task.state(), TaskState::Running);
            task.poll()?;
            if was_running && !matches!(task.state(), TaskState::Running) {
                finished.push(index);
            }
        }
        for index in finished.into_iter().rev() {
            let (task, started) = self.background_tasks.swap_remove(index);
            let success = matches!(
                task.state(),
                TaskState::Finished(mtide_task::ExitStatusCode(Some(0)))
            );
            let label = task.label().to_owned();
            self.remember_task_output(&task);
            self.notification = Some(format!(
                "background task {label} {} ({:.2}s)",
                if success { "succeeded" } else { "finished" },
                started.elapsed().as_secs_f64()
            ));
        }
        Ok(())
    }

    fn selected_task_output(&self) -> Option<(String, String, Option<PathBuf>)> {
        if self.task_output_tab == 0 {
            let task = self.task_run.as_ref()?;
            return Some((
                task.label().to_owned(),
                task.output().to_owned(),
                task.cwd().map(Path::to_path_buf),
            ));
        }
        let index = self
            .task_output_history
            .len()
            .checked_sub(self.task_output_tab)?;
        let snapshot = self.task_output_history.get(index)?;
        Some((
            snapshot.label.clone(),
            snapshot.output.clone(),
            snapshot.cwd.clone(),
        ))
    }

    fn copy_task_output(&mut self) {
        let Some((_, output, _)) = self.selected_task_output() else {
            return;
        };
        if output.is_empty() {
            self.notification = Some("task output is empty".into());
            return;
        }
        self.clipboard = output.clone();
        self.clipboard_history.push(output);
        self.notification = Some("task output copied".into());
    }

    fn jump_to_task_link(&mut self) -> Result<(), AppError> {
        let Some((_, output, task_cwd)) = self.selected_task_output() else {
            self.notification = Some("no task output available".into());
            return Ok(());
        };
        let target = output
            .lines()
            .flat_map(extract_links)
            .map(|link| link.target)
            .find(|target| matches!(target, TerminalLinkTarget::File { .. }));
        let Some(TerminalLinkTarget::File { path, line, column }) = target else {
            self.notification = Some("no file link in task output".into());
            return Ok(());
        };
        let cwd = task_cwd
            .or_else(|| {
                self.path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            })
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let target = PathBuf::from(&path);
        let file = if target.is_absolute() {
            target
        } else {
            cwd.join(target)
        };
        let bytes = fs::read(&file)?;
        let (encoding, text) = decode_text(&bytes).ok_or_else(|| AppError::Binary(file.clone()))?;
        self.navigation.push(self.current_location());
        self.path = Some(file.clone());
        self.document = Document::new(DocId(1), &text);
        self.cursor = self
            .document
            .line_column_to_byte(
                line.saturating_sub(1),
                column.unwrap_or(1).saturating_sub(1),
            )
            .unwrap_or(0);
        self.selection = None;
        self.encoding = encoding;
        self.line_ending = mtide_workspace::detect_line_ending(&text);
        self.baseline = fingerprint(&file).ok();
        self.watcher = file
            .parent()
            .and_then(|parent| WorkspaceWatcher::new(parent).ok());
        self.git_repository = repository_for_path(&file);
        self.git_branch = None;
        self.git_counts = (0, 0, 0, 0);
        self.last_git_poll = Instant::now();
        self.notification = Some(format!("jumped to task output link {path}:{line}"));
        Ok(())
    }

    fn jump_to_first_problem(&mut self) -> Result<(), AppError> {
        self.jump_to_problem_index(0)
    }

    fn jump_to_problem_index(&mut self, index: usize) -> Result<(), AppError> {
        let Some((problem, cwd)) = self.task_run.as_ref().and_then(|task| {
            task.problems()
                .get(index)
                .cloned()
                .map(|problem| (problem, task.cwd().map(Path::to_path_buf)))
        }) else {
            self.notification = Some("no task diagnostics available".into());
            return Ok(());
        };
        let base = cwd
            .or_else(|| {
                self.git_repository
                    .as_ref()
                    .map(|repo| repo.root().to_path_buf())
            })
            .or_else(|| {
                self.path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            })
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let problem_file = problem.file.clone();
        let path = if problem_file.is_absolute() {
            problem_file
        } else {
            base.join(problem_file)
        };
        let bytes = read_file_cancellable(&path, 256 * 1024 * 1024, &AtomicBool::new(false))?;
        let (encoding, text) = decode_text(&bytes).ok_or_else(|| AppError::Binary(path.clone()))?;
        let document = Document::new(DocId(1), &text);
        let cursor = document
            .line_column_to_byte(
                problem.line.saturating_sub(1),
                problem.column.unwrap_or(1).saturating_sub(1),
            )
            .unwrap_or(0);
        self.document = document;
        self.path = Some(path.clone());
        self.cursor = cursor;
        self.selection = None;
        self.extra_cursors.clear();
        self.encoding = encoding;
        self.line_ending = mtide_workspace::detect_line_ending(&text);
        self.baseline = fingerprint(&path).ok();
        self.watcher = path
            .parent()
            .and_then(|parent| WorkspaceWatcher::new(parent).ok());
        self.git_repository = repository_for_path(&path);
        self.git_branch = None;
        self.git_counts = (0, 0, 0, 0);
        self.last_git_poll = Instant::now();
        self.notification = Some(format!(
            "jumped to {}:{}",
            problem.file.display(),
            problem.line
        ));
        Ok(())
    }

    fn draw_terminal_fullscreen(&self, frame: &mut CellBuffer, width: u16, height: u16) {
        let Some(session) = self.terminal_panel.sessions.selected() else {
            return;
        };
        for row in 0..height as usize {
            for column in 0..width as usize {
                if let Some(cell) = session.screen.cell(row, column) {
                    frame.set(
                        column as u16,
                        row as u16,
                        Cell {
                            symbol: if cell.continuation {
                                ' '
                            } else {
                                cell.character
                            },
                            style: terminal_color_slot(if cell.style.inverse {
                                cell.style.background
                            } else {
                                cell.style.foreground
                            }),
                            background: terminal_background_slot(if cell.style.inverse {
                                cell.style.foreground
                            } else {
                                cell.style.background
                            }),
                        },
                    );
                }
            }
        }
        frame.write_str(
            0,
            height.saturating_sub(1),
            &format!(" {}  Alt+T/Ctrl+` exit fullscreen ", session.name()),
            mtide_api::ColorSlot::Focus,
        );
        let _ = width;
    }

    fn toggle_terminal(&mut self) -> Result<(), AppError> {
        if self.terminal_panel.sessions.is_empty() {
            self.new_terminal_session()?;
        }
        let focused = !self.terminal_panel.is_focused();
        self.terminal_panel.set_focus(focused);
        self.notification = Some(if focused {
            "terminal focused".into()
        } else {
            "editor focused".into()
        });
        Ok(())
    }

    fn new_terminal_session(&mut self) -> Result<(), AppError> {
        let shell = env::var("SHELL").unwrap_or_else(|_| "sh".into());
        let name = if self.terminal_panel.sessions.is_empty() {
            "shell".to_owned()
        } else {
            format!("shell-{}", self.terminal_panel.sessions.len() + 1)
        };
        #[cfg(unix)]
        let session = TerminalSession::spawn_pty(
            &name,
            &shell,
            &[],
            self.path.as_deref().and_then(Path::parent),
            80,
            6,
            self.terminal_scrollback,
        )?;
        #[cfg(not(unix))]
        let session = TerminalSession::spawn(
            &name,
            &shell,
            &[],
            self.path.as_deref().and_then(Path::parent),
            80,
            6,
            self.terminal_scrollback,
        )?;
        self.terminal_panel.sessions.add(session);
        self.terminal_panel.set_focus(true);
        self.notification = Some(format!("created terminal session {name}"));
        Ok(())
    }

    fn close_terminal_session(&mut self) {
        let Some(session) = self.terminal_panel.sessions.close_selected() else {
            self.notification = Some("no terminal session to close".into());
            return;
        };
        self.notification = Some(format!("closed terminal session {}", session.name()));
        if self.terminal_panel.sessions.is_empty() {
            self.terminal_panel.set_focus(false);
        }
    }

    fn open_terminal_rename(&mut self) {
        let Some(session) = self.terminal_panel.sessions.selected() else {
            self.notification = Some("no terminal session to rename".into());
            return;
        };
        self.terminal_rename_input = session.name().to_owned();
        self.terminal_rename_open = true;
    }

    fn apply_terminal_rename(&mut self) {
        let name = self.terminal_rename_input.trim().to_owned();
        let Some(session) = self.terminal_panel.sessions.selected_mut() else {
            self.notification = Some("no terminal session to rename".into());
            return;
        };
        match session.rename(name.clone()) {
            Ok(()) => {
                self.notification = Some(format!("renamed terminal session to {name}"));
                self.terminal_rename_open = false;
                self.terminal_rename_input.clear();
            }
            Err(error) => self.notification = Some(format!("terminal rename failed: {error}")),
        }
    }

    fn save_workspace_file(&mut self) -> Result<(), AppError> {
        let path = self.workspace_file.clone().unwrap_or_else(|| {
            self.session_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("workspace.mtide")
        });
        let mut workspace = WorkspaceFile::from_roots(&path, &self.workspace_roots);
        if let Some(width) = self.sidebar_width {
            workspace
                .layout
                .insert("sidebar.width".into(), width.to_string());
        }
        workspace.layout.insert("theme".into(), self.theme.clone());
        workspace
            .layout
            .insert("keymap".into(), self.keymap_preset.clone());
        if let Some(path) = &self.keymap_import_path {
            workspace
                .layout
                .insert("keymap.vimrc".into(), path.to_string_lossy().into_owned());
        }
        workspace.layout.insert(
            "editor.split".into(),
            if self.editor_split { "true" } else { "false" }.into(),
        );
        workspace.layout.insert(
            "editor.split.direction".into(),
            if self.editor_split_horizontal {
                "horizontal"
            } else {
                "vertical"
            }
            .into(),
        );
        workspace.save(&path)?;
        self.workspace_file = Some(path);
        self.notification = Some("workspace saved".into());
        Ok(())
    }

    fn export_diagnostics_bundle(&mut self) -> Result<(), AppError> {
        let target = self
            .session_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("mtide-diagnostics.json.gz");
        let capabilities = TerminalCapabilities::detect();
        let mut notes = vec![
            format!("workspace roots: {}", self.workspace_roots.len()),
            format!("open tabs: {}", self.tabs.len()),
            format!("git detected: {}", self.git_repository.is_some()),
            format!("terminal color: {:?}", capabilities.color),
            format!("terminal unicode: {}", capabilities.unicode),
            "source text, prompts, and sensitive configuration values omitted".into(),
        ];
        if let Some(path) = &self.path {
            notes.push(format!("active file: {}", redacted_path(path)));
        }
        let report = DiagnosticReport {
            version: env!("CARGO_PKG_VERSION").into(),
            platform: std::env::consts::OS.into(),
            terminal: format!("{:?}", capabilities),
            config_files: vec!["user config".into(), "project config".into()],
            notes: std::mem::take(&mut notes),
        };
        export_diagnostics(&target, &report)?;
        self.notification = Some(format!("diagnostics exported to {}", target.display()));
        Ok(())
    }

    fn open_workspace_root_input(&mut self, remove: bool) {
        self.workspace_root_input_open = true;
        self.workspace_root_input_remove = remove;
        self.workspace_root_input.clear();
    }

    fn apply_workspace_root_input(&mut self) -> Result<(), AppError> {
        let input = self.workspace_root_input.trim();
        if input.is_empty() {
            self.notification = Some("workspace root path must not be empty".into());
            return Ok(());
        }
        let path = PathBuf::from(input);
        let path = if path.is_absolute() {
            path
        } else {
            env::current_dir()?.join(path)
        };
        let path = fs::canonicalize(&path).map_err(AppError::Io)?;
        if !path.is_dir() {
            self.notification = Some(format!(
                "workspace root is not a directory: {}",
                path.display()
            ));
            return Ok(());
        }
        if self.workspace_root_input_remove {
            if let Some(index) = self.workspace_roots.iter().position(|root| root == &path) {
                self.workspace_roots.remove(index);
                self.notification = Some(format!("removed workspace root: {}", path.display()));
            } else {
                self.notification = Some(format!("workspace root not found: {}", path.display()));
            }
        } else if self.workspace_roots.contains(&path) {
            self.notification = Some("workspace root already exists".into());
        } else {
            self.workspace_roots.push(path.clone());
            self.notification = Some(format!("added workspace root: {}", path.display()));
        }
        self.workspace_root_input_open = false;
        self.workspace_root_input.clear();
        self.save_workspace_file()?;
        Ok(())
    }

    fn route_terminal_key(&mut self, key: KeyEvent) -> Result<bool, AppError> {
        if !self.terminal_panel.is_focused()
            && !self.git_panel_open
            && !self.git_commit_open
            && !self.git_branch_input_open
            && !self.workspace_root_input_open
            && !self.terminal_rename_open
            && !self.save_as_open
            && !self.git_diff_open
            && !self.task_output_open
            && !self.task_results_open
        {
            return Ok(false);
        }
        if self.terminal_rename_open {
            match key.code {
                KeyCode::Esc => {
                    self.terminal_rename_open = false;
                    self.terminal_rename_input.clear();
                }
                KeyCode::Enter => self.apply_terminal_rename(),
                KeyCode::Backspace => {
                    self.terminal_rename_input.pop();
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.terminal_rename_input.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.save_as_open {
            match key.code {
                KeyCode::Esc => {
                    self.save_as_open = false;
                    self.save_as_input.clear();
                }
                KeyCode::Enter => self.apply_save_as()?,
                KeyCode::Backspace => {
                    self.save_as_input.pop();
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.save_as_input.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.terminal_panel.is_focused() {
            if key.code == KeyCode::Char('r')
                && key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                self.open_terminal_rename();
                return Ok(false);
            }
            if key.code == KeyCode::Char('n')
                && key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                self.new_terminal_session()?;
                return Ok(false);
            }
            if key.code == KeyCode::Char('x')
                && key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                self.close_terminal_session();
                return Ok(false);
            }
            if key.code == KeyCode::PageUp && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.terminal_panel.sessions.previous();
                return Ok(false);
            }
            if key.code == KeyCode::PageDown && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.terminal_panel.sessions.next();
                return Ok(false);
            }
        }
        if self.git_commit_open {
            match key.code {
                KeyCode::Esc => {
                    self.git_commit_open = false;
                    self.git_commit_message.clear();
                    self.git_commit_amend = false;
                    self.git_commit_feedback.clear();
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.git_commit_message.push('\n');
                }
                KeyCode::Enter => self.commit_git_changes()?,
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.git_commit_amend = !self.git_commit_amend;
                }
                KeyCode::Backspace => {
                    self.git_commit_message.pop();
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.git_commit_message.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.workspace_root_input_open {
            match key.code {
                KeyCode::Esc => {
                    self.workspace_root_input_open = false;
                    self.workspace_root_input.clear();
                }
                KeyCode::Enter => self.apply_workspace_root_input()?,
                KeyCode::Backspace => {
                    self.workspace_root_input.pop();
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.workspace_root_input.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.git_branch_input_open {
            match key.code {
                KeyCode::Esc => {
                    self.git_branch_input_open = false;
                    self.git_branch_input.clear();
                }
                KeyCode::Enter => self.apply_git_branch_input()?,
                KeyCode::Backspace => {
                    self.git_branch_input.pop();
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.git_branch_input.push(character);
                }
                _ => {}
            }
            return Ok(false);
        }
        if let Some(branch) = self.git_checkout_pending.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.git_checkout_pending = None;
                    self.notification = Some("branch checkout cancelled".into());
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.git_checkout_pending = None;
                    self.checkout_git_branch(&branch, GitCheckoutMode::Stash)?;
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    self.git_checkout_pending = None;
                    self.checkout_git_branch(&branch, GitCheckoutMode::Carry)?;
                }
                KeyCode::Char('[') => {
                    self.git_diff_context = self.git_diff_context.saturating_sub(1);
                    self.git_diff_scroll = 0;
                }
                KeyCode::Char(']') => {
                    self.git_diff_context = (self.git_diff_context + 1).min(50);
                    self.git_diff_scroll = 0;
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.git_diff_open {
            match key.code {
                KeyCode::Esc | KeyCode::F(7) => {
                    if let Some(cancellation) = &self.git_diff_cancel {
                        cancellation.cancel();
                    }
                    self.git_diff_open = false;
                    self.git_diff_lines.clear();
                    self.git_diff_line_hunks.clear();
                    self.git_diff_hunks.clear();
                    self.git_diff_selected_lines.clear();
                    self.git_diff_data = None;
                    self.git_diff_path = None;
                }
                KeyCode::Tab if !self.git_diff_hunks.is_empty() => {
                    self.git_diff_hunk_selected =
                        (self.git_diff_hunk_selected + 1) % self.git_diff_hunks.len();
                    if let Some(line) = self
                        .git_diff_line_hunks
                        .iter()
                        .position(|hunk| *hunk == Some(self.git_diff_hunk_selected))
                    {
                        self.git_diff_scroll = line;
                    }
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    self.git_diff_split = !self.git_diff_split;
                    self.git_diff_scroll = 0;
                    self.notification = Some(if self.git_diff_split {
                        "Git diff split view enabled (narrow terminals use unified view)".into()
                    } else {
                        "Git diff unified view enabled".into()
                    });
                }
                KeyCode::Char('w') | KeyCode::Char('W') => {
                    self.git_diff_ignore_whitespace = !self.git_diff_ignore_whitespace;
                    self.open_git_diff();
                    self.notification = Some(if self.git_diff_ignore_whitespace {
                        "Git diff now ignores whitespace changes".into()
                    } else {
                        "Git diff now includes whitespace changes".into()
                    });
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    self.git_diff_staged = !self.git_diff_staged;
                    self.open_git_diff();
                    self.notification = Some(if self.git_diff_staged {
                        "Git diff now shows staged changes".into()
                    } else {
                        "Git diff now shows working-tree changes".into()
                    });
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    let line = self.git_diff_scroll;
                    if self
                        .git_diff_lines
                        .get(line)
                        .is_some_and(|(_, kind)| !matches!(kind, GitDiffLineKind::Equal))
                        && !self.git_diff_selected_lines.insert(line)
                    {
                        self.git_diff_selected_lines.remove(&line);
                    }
                }
                KeyCode::Char('s') => {
                    if self.git_diff_selected_lines.is_empty() {
                        if self.git_diff_staged {
                            self.unstage_selected_git_hunk()?;
                        } else {
                            self.stage_selected_git_hunk()?;
                        }
                    } else {
                        if self.git_diff_staged {
                            self.unstage_selected_git_lines()?;
                        } else {
                            self.stage_selected_git_lines()?;
                        }
                    }
                }
                KeyCode::Up => self.git_diff_scroll = self.git_diff_scroll.saturating_sub(1),
                KeyCode::Down => {
                    self.git_diff_scroll =
                        (self.git_diff_scroll + 1).min(self.git_diff_lines.len().saturating_sub(1));
                }
                KeyCode::PageUp => self.git_diff_scroll = self.git_diff_scroll.saturating_sub(10),
                KeyCode::PageDown => {
                    self.git_diff_scroll = (self.git_diff_scroll + 10)
                        .min(self.git_diff_lines.len().saturating_sub(1));
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.task_output_open {
            if let Some(query) = self.task_output_search.as_mut() {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => self.task_output_search = None,
                    KeyCode::Backspace => {
                        query.pop();
                    }
                    KeyCode::Char(character)
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        query.push(character);
                    }
                    _ => {}
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc | KeyCode::F(5) => self.task_output_open = false,
                KeyCode::Char('/') => self.task_output_search = Some(String::new()),
                KeyCode::Char('y') | KeyCode::Char('Y') => self.copy_task_output(),
                KeyCode::Char('l') | KeyCode::Char('L') => self.jump_to_task_link()?,
                KeyCode::Tab | KeyCode::Right if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.task_output_tab =
                        (self.task_output_tab + 1).min(self.task_output_history.len());
                    self.task_output_scroll = 0;
                }
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left
                    if key.modifiers.contains(KeyModifiers::SHIFT) || key.code == KeyCode::Left =>
                {
                    self.task_output_tab = self.task_output_tab.saturating_sub(1);
                    self.task_output_scroll = 0;
                }
                KeyCode::Up => self.task_output_scroll = self.task_output_scroll.saturating_sub(1),
                KeyCode::Down => {
                    self.task_output_scroll = self.task_output_scroll.saturating_add(1)
                }
                KeyCode::PageUp => {
                    self.task_output_scroll = self.task_output_scroll.saturating_sub(10)
                }
                KeyCode::PageDown => {
                    self.task_output_scroll = self.task_output_scroll.saturating_add(10)
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.git_editor_original_open {
            match key.code {
                KeyCode::Esc => self.git_editor_original_open = false,
                KeyCode::Up => {
                    self.git_editor_original_scroll =
                        self.git_editor_original_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    self.git_editor_original_scroll =
                        self.git_editor_original_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    self.git_editor_original_scroll =
                        self.git_editor_original_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    self.git_editor_original_scroll =
                        self.git_editor_original_scroll.saturating_add(10);
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.git_history_detail_open {
            match key.code {
                KeyCode::Esc | KeyCode::F(7) => {
                    if let Some(cancellation) = &self.git_history_detail_cancel {
                        cancellation.cancel();
                    }
                    self.git_history_detail_open = false;
                    self.git_history_detail = None;
                }
                KeyCode::Up => {
                    self.git_history_detail_scroll =
                        self.git_history_detail_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    self.git_history_detail_scroll =
                        self.git_history_detail_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    self.git_history_detail_scroll =
                        self.git_history_detail_scroll.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    self.git_history_detail_scroll =
                        self.git_history_detail_scroll.saturating_add(10);
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.task_results_open {
            match key.code {
                KeyCode::Esc | KeyCode::F(6) => self.task_results_open = false,
                KeyCode::Up => {
                    self.task_result_selected = self.task_result_selected.saturating_sub(1)
                }
                KeyCode::Down => {
                    let count = self
                        .task_run
                        .as_ref()
                        .map(|task| task.result_tree().children.len() + task.problems().len())
                        .unwrap_or(0);
                    if count > 0 {
                        self.task_result_selected = (self.task_result_selected + 1).min(count - 1);
                    }
                }
                KeyCode::Enter => {
                    let result_count = self
                        .task_run
                        .as_ref()
                        .map(|task| task.result_tree().children.len())
                        .unwrap_or(0);
                    self.task_results_open = false;
                    if self.task_result_selected >= result_count {
                        self.jump_to_problem_index(self.task_result_selected - result_count)?;
                    } else {
                        self.notification =
                            Some("selected task result has no source location".into());
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.git_panel_open {
            if self.git_history_filter_open {
                match key.code {
                    KeyCode::Esc => {
                        self.git_history_filter_open = false;
                        self.git_history_filter.clear();
                    }
                    KeyCode::Enter => {
                        self.git_history_filter_open = false;
                        self.refresh_git_history();
                    }
                    KeyCode::Backspace => {
                        self.git_history_filter.pop();
                    }
                    KeyCode::Char(character)
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        self.git_history_filter.push(character);
                    }
                    _ => {}
                }
                return Ok(false);
            }
            if key.code == KeyCode::Char('s') || key.code == KeyCode::Char('S') {
                self.git_stash_mode = !self.git_stash_mode;
                self.git_branch_mode = false;
                self.git_history_mode = false;
                if self.git_stash_mode {
                    self.git_stash_selected = 0;
                    self.last_git_poll = Instant::now() - Duration::from_secs(1);
                }
                return Ok(false);
            }
            if key.code == KeyCode::Char('h') || key.code == KeyCode::Char('H') {
                self.git_history_mode = !self.git_history_mode;
                self.git_branch_mode = false;
                self.git_stash_mode = false;
                if self.git_history_mode {
                    self.git_history_selected = 0;
                    self.refresh_git_history();
                }
                return Ok(false);
            }
            if self.git_history_mode && key.modifiers.is_empty() && key.code == KeyCode::Char('/') {
                self.git_history_filter_kind = GitHistoryFilterKind::Keyword;
                self.git_history_filter_open = true;
                return Ok(false);
            }
            if self.git_history_mode && key.modifiers.is_empty() && key.code == KeyCode::Char('a') {
                self.git_history_filter_kind = GitHistoryFilterKind::Author;
                self.git_history_filter_open = true;
                return Ok(false);
            }
            if self.git_history_mode && key.modifiers.is_empty() && key.code == KeyCode::Char('p') {
                self.git_history_file_only = !self.git_history_file_only;
                self.refresh_git_history();
                self.notification = Some(
                    if self.git_history_file_only {
                        "Git history filtered to current file"
                    } else {
                        "Git history file filter disabled"
                    }
                    .into(),
                );
                return Ok(false);
            }
            if key.code == KeyCode::Char('b') || key.code == KeyCode::Char('B') {
                self.git_branch_mode = !self.git_branch_mode;
                self.git_history_mode = false;
                self.git_stash_mode = false;
                if self.git_branch_mode {
                    self.git_branch_selected = 0;
                }
                return Ok(false);
            }
            if self.git_branch_mode && key.code == KeyCode::Char('n') && key.modifiers.is_empty() {
                self.git_branch_input_open = true;
                self.git_branch_input_rename = false;
                self.git_branch_input.clear();
                return Ok(false);
            }
            if self.git_branch_mode && key.code == KeyCode::Char('r') && key.modifiers.is_empty() {
                if self
                    .git_branches
                    .get(self.git_branch_selected)
                    .is_some_and(|branch| !branch.remote)
                {
                    self.git_branch_input_open = true;
                    self.git_branch_input_rename = true;
                    self.git_branch_input.clear();
                } else {
                    self.notification = Some("remote branches cannot be renamed here".into());
                }
                return Ok(false);
            }
            if self.git_branch_mode && key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
                let Some(branch) = self.git_branches.get(self.git_branch_selected).cloned() else {
                    return Ok(false);
                };
                if branch.remote || branch.current {
                    self.notification =
                        Some("cannot delete the current or remote branch from this view".into());
                } else if self.git_branch_delete_pending.as_deref() == Some(branch.name.as_str()) {
                    self.delete_git_branch(&branch.name)?;
                    self.git_branch_delete_pending = None;
                } else {
                    self.git_branch_delete_pending = Some(branch.name.clone());
                    self.notification = Some(format!(
                        "delete {}? press d again to confirm, Esc to cancel",
                        branch.name
                    ));
                }
                return Ok(false);
            }
            if self.git_branch_mode
                && key.modifiers.is_empty()
                && matches!(key.code, KeyCode::Char('m') | KeyCode::Char('R'))
            {
                if let Some(branch) = self.git_branches.get(self.git_branch_selected) {
                    if branch.current {
                        self.notification = Some("select a different branch first".into());
                    } else {
                        let action = if key.code == KeyCode::Char('m') {
                            GitBranchAction::Merge
                        } else {
                            GitBranchAction::Rebase
                        };
                        self.start_git_branch_action(action, branch.name.clone());
                    }
                }
                return Ok(false);
            }
            if !self.git_branch_mode
                && !self.git_history_mode
                && !self.git_stash_mode
                && key.modifiers.is_empty()
                && key.code == KeyCode::Char('t')
            {
                self.git_tree_mode = !self.git_tree_mode;
                self.notification = Some(
                    if self.git_tree_mode {
                        "Git changes tree view enabled"
                    } else {
                        "Git changes list view enabled"
                    }
                    .into(),
                );
                return Ok(false);
            }
            if !self.git_branch_mode
                && !self.git_history_mode
                && !self.git_stash_mode
                && key.modifiers.is_empty()
            {
                let action = match key.code {
                    KeyCode::Char('f') => Some(GitRemoteAction::Fetch),
                    KeyCode::Char('l') => Some(GitRemoteAction::Pull),
                    KeyCode::Char('u') => Some(GitRemoteAction::Push),
                    KeyCode::Char('y') => Some(GitRemoteAction::Sync),
                    _ => None,
                };
                if let Some(action) = action {
                    self.start_git_remote(action);
                    return Ok(false);
                }
            }
            if key.code == KeyCode::Char('c')
                && key.modifiers.is_empty()
                && !self.git_branch_mode
                && !self.git_history_mode
                && !self.git_stash_mode
            {
                self.git_commit_open = true;
                self.git_commit_message.clear();
                self.git_commit_amend = false;
                self.git_commit_feedback.clear();
                return Ok(false);
            }
            if key.code == KeyCode::Char('a') && key.modifiers.is_empty() && !self.git_stash_mode {
                self.stage_all_git_entries()?;
                return Ok(false);
            }
            if key.code == KeyCode::Char('A') && key.modifiers.is_empty() && !self.git_stash_mode {
                self.unstage_all_git_entries()?;
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc | KeyCode::F(7) => {
                    if let Some(cancellation) = &self.git_status_cancel {
                        cancellation.cancel();
                    }
                    self.git_panel_open = false;
                    self.git_branch_mode = false;
                    self.git_history_mode = false;
                    self.git_stash_mode = false;
                    self.git_branch_delete_pending = None;
                    self.git_checkout_pending = None;
                    self.git_history_filter_open = false;
                }
                KeyCode::Up if self.git_stash_mode => {
                    self.git_stash_selected = self.git_stash_selected.saturating_sub(1)
                }
                KeyCode::Down if self.git_stash_mode => {
                    if !self.git_stashes.is_empty() {
                        self.git_stash_selected = (self.git_stash_selected + 1)
                            .min(self.git_stashes.len().saturating_sub(1));
                    }
                }
                KeyCode::Char('p') if self.git_stash_mode => self.apply_git_stash(false)?,
                KeyCode::Char('o') if self.git_stash_mode => self.apply_git_stash(true)?,
                KeyCode::Char('d') if self.git_stash_mode => self.drop_git_stash()?,
                KeyCode::Char('n') if self.git_stash_mode => self.create_git_stash()?,
                KeyCode::Enter if self.git_stash_mode => self.open_git_stash_detail(),
                KeyCode::Up if self.git_history_mode => {
                    self.git_history_selected = self.git_history_selected.saturating_sub(1)
                }
                KeyCode::Down if self.git_history_mode => {
                    if !self.git_history.is_empty() {
                        self.git_history_selected = (self.git_history_selected + 1)
                            .min(self.git_history.len().saturating_sub(1));
                    }
                }
                KeyCode::Enter if self.git_history_mode => {
                    self.open_git_history_detail();
                }
                KeyCode::PageDown if self.git_history_mode => {
                    self.git_history_offset = self.git_history.len();
                    self.git_history_requested = true;
                    self.last_git_poll = Instant::now() - Duration::from_secs(1);
                }
                KeyCode::Up if self.git_branch_mode => {
                    self.git_branch_selected = self.git_branch_selected.saturating_sub(1)
                }
                KeyCode::Down if self.git_branch_mode => {
                    if !self.git_branches.is_empty() {
                        self.git_branch_selected = (self.git_branch_selected + 1)
                            .min(self.git_branches.len().saturating_sub(1));
                    }
                }
                KeyCode::Enter if self.git_branch_mode => {
                    if let Some(branch_name) = self
                        .git_branches
                        .get(self.git_branch_selected)
                        .map(|branch| branch.name.clone())
                    {
                        self.checkout_git_branch(&branch_name, GitCheckoutMode::Prompt)?;
                    }
                }
                KeyCode::Enter if !self.git_history_mode && !self.git_stash_mode => {
                    self.open_git_diff();
                }
                KeyCode::Up => self.git_selected = self.git_selected.saturating_sub(1),
                KeyCode::Down => {
                    if !self.git_entries.is_empty() {
                        self.git_selected =
                            (self.git_selected + 1).min(self.git_entries.len().saturating_sub(1));
                    }
                }
                KeyCode::Char(' ') => self.toggle_git_entry_stage()?,
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.last_git_poll = Instant::now() - Duration::from_secs(1);
                }
                _ => {}
            }
            return Ok(false);
        }
        let Some(stroke) = key_stroke(key) else {
            return Ok(false);
        };
        if !self.terminal_panel.is_fullscreen()
            && self.terminal_panel.key_policy.route(stroke) != RoutedInput::Terminal
        {
            return Ok(false);
        }
        let Some(bytes) = terminal_bytes(key) else {
            return Ok(false);
        };
        if let Some(session) = self.terminal_panel.sessions.selected_mut() {
            session.send_input(&bytes)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn send_selection_to_terminal(&mut self, auto_enter: bool) -> Result<(), AppError> {
        let Some(selection) = self.selection else {
            self.notification = Some("select text before sending to terminal".into());
            return Ok(());
        };
        let text = self.document.text();
        let Some(selected) = text.get(selection.range()) else {
            self.notification = Some("selection is not valid UTF-8 text".into());
            return Ok(());
        };
        if self.terminal_panel.sessions.is_empty() {
            self.toggle_terminal()?;
        }
        if let Some(session) = self.terminal_panel.sessions.selected_mut() {
            session.send_input(selected.as_bytes())?;
            if auto_enter {
                session.send_input(b"\r")?;
            }
            self.terminal_panel.set_focus(true);
            self.notification = Some(if auto_enter {
                "selection sent to terminal and executed".into()
            } else {
                "selection sent to terminal".into()
            });
        }
        Ok(())
    }

    fn jump_to_terminal_link(&mut self) -> Result<(), AppError> {
        let target = self.terminal_panel.sessions.selected().and_then(|session| {
            session
                .scrollback()
                .flat_map(extract_links)
                .map(|link| link.target)
                .next()
        });
        let Some(target) = target else {
            self.notification = Some("no file or URL link in terminal output".into());
            return Ok(());
        };
        match target {
            TerminalLinkTarget::Url(url) => {
                let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
                    ("open", vec![url.as_str()])
                } else if cfg!(target_os = "windows") {
                    ("cmd", vec!["/C", "start", "", url.as_str()])
                } else {
                    ("xdg-open", vec![url.as_str()])
                };
                Command::new(program).args(args).spawn()?;
                self.notification = Some("opened terminal URL link".into());
            }
            TerminalLinkTarget::File { path, line, column } => {
                let cwd = self
                    .terminal_panel
                    .sessions
                    .selected()
                    .and_then(|session| session.cwd().map(Path::to_path_buf))
                    .or_else(|| {
                        self.path
                            .as_deref()
                            .and_then(Path::parent)
                            .map(Path::to_path_buf)
                    })
                    .or_else(|| env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                let target = PathBuf::from(&path);
                let file = if target.is_absolute() {
                    target
                } else {
                    cwd.join(target)
                };
                let bytes = fs::read(&file)?;
                let (encoding, text) =
                    decode_text(&bytes).ok_or_else(|| AppError::Binary(file.clone()))?;
                self.navigation.push(self.current_location());
                self.path = Some(file.clone());
                self.document = Document::new(DocId(1), &text);
                self.cursor = self
                    .document
                    .line_column_to_byte(
                        line.saturating_sub(1),
                        column.unwrap_or(1).saturating_sub(1),
                    )
                    .unwrap_or(0);
                self.selection = None;
                self.encoding = encoding;
                self.line_ending = mtide_workspace::detect_line_ending(&text);
                self.baseline = fingerprint(&file).ok();
                self.watcher = file
                    .parent()
                    .and_then(|parent| WorkspaceWatcher::new(parent).ok());
                self.git_repository = repository_for_path(&file);
                self.git_branch = None;
                self.git_counts = (0, 0, 0, 0);
                self.last_git_poll = Instant::now();
                self.notification = Some(format!("jumped to {path}:{line}"));
            }
        }
        Ok(())
    }

    fn status_line(&self, width: usize, byte_len: usize) -> String {
        let path = self
            .path
            .as_deref()
            .and_then(Path::to_str)
            .unwrap_or("[untitled]");
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0) + 1;
        let column = self.current_column() + 1;
        let encoding = match self.encoding {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf8Bom => "UTF-8 BOM",
            TextEncoding::Utf16Le => "UTF-16 LE",
            TextEncoding::Utf16Be => "UTF-16 BE",
            TextEncoding::Latin1 => "Latin-1",
            TextEncoding::Gbk => "GBK",
            TextEncoding::Binary => "binary",
        };
        let ending = match self.line_ending {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
            LineEnding::Cr => "CR",
            LineEnding::Mixed => "mixed",
            LineEnding::None => "none",
        };
        let indent = if self.insert_spaces {
            format!("spaces:{}", self.tab_width)
        } else {
            "tabs".to_owned()
        };
        let mut status = format!(
            " {}{}  {} bytes  Ln {}, Col {}  {} {}  {}  wrap={}  theme={}  keys={}",
            path,
            if self.document.is_dirty() { " *" } else { "" },
            byte_len,
            line,
            column,
            encoding,
            ending,
            indent,
            if self.word_wrap { "on" } else { "off" },
            self.theme,
            self.keymap_preset
        );
        if self.readonly {
            status.push_str("  readonly");
        }
        if self.large_file_mode {
            status.push_str("  large-file");
        }
        if self.config_warning {
            status.push_str("  config:warning");
        }
        if self.external_change {
            status.push_str("  external-change");
        }
        if !self.background_tasks.is_empty() {
            status.push_str(&format!("  background:{}", self.background_tasks.len()));
        }
        if self.scrollbar_mode == "hidden" {
            status.push_str("  scroll:hidden");
        }
        if let Some(branch) = &self.git_branch {
            let (staged, unstaged, untracked, conflicts) = self.git_counts;
            status.push_str(&format!(
                "  git:{branch} +{staged} ~{unstaged} ?{untracked}{}",
                if conflicts > 0 {
                    format!(" !{conflicts}")
                } else {
                    String::new()
                }
            ));
        }
        if self.git_editor_diff_mode {
            status.push_str("  diff:edit");
        }
        if let Some(blame) = &self.git_blame {
            status.push_str("  blame:");
            status.push_str(blame);
        }
        if let Some(notification) = &self.notification {
            status.push_str("  ");
            status.push_str(notification);
        }
        status.graphemes(true).take(width).collect()
    }

    fn poll_workspace_changes(&mut self) {
        if self.last_watch_poll.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_watch_poll = Instant::now();
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let Ok(changes) = watcher.poll() else {
            return;
        };
        let Some(path) = self.path.clone() else {
            return;
        };
        let kind = changes
            .iter()
            .find(|change| change.path == path)
            .map(|change| change.kind.clone())
            .or_else(|| {
                let baseline = self.baseline.as_ref()?;
                if path.exists() {
                    changed_since(&path, baseline)
                        .ok()
                        .filter(|changed| *changed)
                        .map(|_| ChangeKind::Modified)
                } else {
                    Some(ChangeKind::Deleted)
                }
            });
        let Some(kind) = kind else { return };
        match kind {
            ChangeKind::Deleted => {
                self.external_change = true;
                self.notification = Some("file deleted externally".into());
            }
            ChangeKind::Modified if self.document.is_dirty() => {
                self.external_change = true;
                self.notification = Some("external change: reload/compare/overwrite".into());
            }
            ChangeKind::Modified => {
                if let Ok(bytes) = fs::read(&path) {
                    if let Some((encoding, text)) = decode_text(&bytes) {
                        self.document = Document::new(DocId(1), &text);
                        self.cursor = self.cursor.min(text.len());
                        self.encoding = encoding;
                        self.line_ending = mtide_workspace::detect_line_ending(&text);
                        self.baseline = fingerprint(&path).ok();
                        self.external_change = false;
                        self.notification = Some("reloaded external change".into());
                    }
                }
            }
            ChangeKind::Created => {}
        }
    }

    fn handle(&mut self, key: KeyEvent) -> Result<bool, AppError> {
        if self.notifications_open {
            if key.code == KeyCode::Esc || key.code == KeyCode::F(8) {
                self.notifications_open = false;
            }
            return Ok(false);
        }
        if self.theme_preview.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.theme_preview = None;
                    self.notification = Some("theme preview cancelled".into());
                }
                KeyCode::Enter => self.apply_theme_preview()?,
                KeyCode::Left | KeyCode::Right => {
                    self.cycle_theme_preview(key.code == KeyCode::Right)
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.terminal_panel.is_focused()
            && key.code == KeyCode::Char('p')
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && self.route_terminal_key(key)?
        {
            return Ok(false);
        }
        if !self.terminal_panel.is_focused() && self.keymap_preset.eq_ignore_ascii_case("vscode") {
            if let Some(started) = self.vscode_chord_started {
                if started.elapsed() > Duration::from_millis(1500) {
                    self.vscode_chord_started = None;
                    self.notification = Some("VSCode chord timed out".into());
                } else if key.code == KeyCode::Esc {
                    self.vscode_chord_started = None;
                    self.notification = Some("VSCode chord cancelled".into());
                    return Ok(false);
                } else if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    self.vscode_chord_started = None;
                    self.key_help = true;
                    self.key_help_query.clear();
                    return Ok(false);
                } else if key.code == KeyCode::Char('z') && key.modifiers.is_empty() {
                    self.vscode_chord_started = None;
                    self.zen_mode = !self.zen_mode;
                    self.notification = Some(
                        if self.zen_mode {
                            "zen mode enabled"
                        } else {
                            "zen mode disabled"
                        }
                        .into(),
                    );
                    return Ok(false);
                } else if key.code == KeyCode::Char('l') && key.modifiers.is_empty() {
                    self.vscode_chord_started = None;
                    self.add_cursors_for_search_matches();
                    return Ok(false);
                } else {
                    self.vscode_chord_started = None;
                    self.notification = Some("unknown VSCode chord".into());
                }
            }
            if key.code == KeyCode::Char('k') && key.modifiers == KeyModifiers::CONTROL {
                self.vscode_chord_started = Some(Instant::now());
                self.notification = Some("VSCode chord: waiting for next key".into());
                return Ok(false);
            }
        }
        if self.file_finder_open {
            match key.code {
                KeyCode::Esc => {
                    self.file_finder_open = false;
                    if let Some(token) = self.file_finder_token.take() {
                        token.cancel();
                    }
                    self.file_finder_job = None;
                }
                KeyCode::Up => {
                    self.file_finder_selected = self.file_finder_selected.saturating_sub(1)
                }
                KeyCode::Down => {
                    if !self.file_finder_results.is_empty() {
                        self.file_finder_selected = (self.file_finder_selected + 1)
                            .min(self.file_finder_results.len().saturating_sub(1));
                    }
                }
                KeyCode::Enter => {
                    if let Some(path) = self
                        .file_finder_results
                        .get(self.file_finder_selected)
                        .cloned()
                    {
                        self.file_finder_open = false;
                        self.open_file_tab(&path, false)?;
                    }
                }
                KeyCode::Backspace => {
                    self.file_finder_query.pop();
                    self.file_finder_selected = 0;
                    self.start_file_finder_search();
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.file_finder_query.push(character);
                    self.file_finder_selected = 0;
                    self.start_file_finder_search();
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.recent_open {
            match key.code {
                KeyCode::Esc => self.recent_open = false,
                KeyCode::Up => self.recent_selected = self.recent_selected.saturating_sub(1),
                KeyCode::Down => {
                    let count = self.filtered_recent_files().len();
                    if count > 0 {
                        self.recent_selected =
                            (self.recent_selected + 1).min(count.saturating_sub(1));
                    }
                }
                KeyCode::Enter => {
                    if let Some(path) = self
                        .filtered_recent_files()
                        .get(self.recent_selected)
                        .cloned()
                    {
                        self.recent_open = false;
                        self.open_file_tab(&path, false)?;
                    }
                }
                KeyCode::Backspace => {
                    self.recent_query.pop();
                    self.recent_selected = 0;
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.recent_query.push(character);
                    self.recent_selected = 0;
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.key_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(1)) {
                self.key_help = false;
                self.key_help_query.clear();
            } else if key.code == KeyCode::Backspace {
                self.key_help_query.pop();
            } else if let KeyCode::Char(character) = key.code {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    self.key_help_query.push(character);
                }
            }
            return Ok(false);
        }
        if !self.keymap_preset.eq_ignore_ascii_case("vscode") && self.keymap.has_binding(key) {
            let action = self.keymap.action(key);
            if !matches!(action, Action::None) {
                return self.apply_keymap_action(action);
            }
        }
        if key.code == KeyCode::F(1) {
            self.key_help = true;
            self.key_help_query.clear();
            return Ok(false);
        }
        if key.code == KeyCode::F(8) {
            self.notifications_open = true;
            return Ok(false);
        }
        if key.code == KeyCode::Char('g')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.cancel_git_operations();
            return Ok(false);
        }
        if key.code == KeyCode::F(7) {
            self.git_panel_open = true;
            self.git_history_requested = true;
            self.last_git_poll = Instant::now() - Duration::from_secs(1);
            return Ok(false);
        }
        if key.code == KeyCode::F(6) {
            self.task_results_open = true;
            self.task_result_selected = 0;
            return Ok(false);
        }
        if key.code == KeyCode::Char('o')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.task_output_open = self.task_run.is_some() || !self.task_output_history.is_empty();
            self.task_output_scroll = 0;
            self.task_output_tab = 0;
            return Ok(false);
        }
        if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_recent_files();
            return Ok(false);
        }
        if key.code == KeyCode::F(11) {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                self.clear_bookmarks();
            } else {
                self.toggle_bookmark();
            }
            return Ok(false);
        }
        if key.code == KeyCode::Char('m')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.toggle_theme()?;
            return Ok(false);
        }
        if self.workspace_search_open {
            if key.code == KeyCode::Char('f')
                && key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                self.workspace_search_regex = !self.workspace_search_regex;
                self.start_workspace_search();
                return Ok(false);
            }
            if key.code == KeyCode::Char('i')
                && key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                self.workspace_search_case_sensitive = !self.workspace_search_case_sensitive;
                self.start_workspace_search();
                return Ok(false);
            }
            if key.code == KeyCode::Char('w')
                && key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                self.workspace_search_whole_word = !self.workspace_search_whole_word;
                self.start_workspace_search();
                return Ok(false);
            }
            if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.workspace_replace_mode = !self.workspace_replace_mode;
                return Ok(false);
            }
            if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.rollback_workspace_replace()?;
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc => {
                    if let Some(token) = self.workspace_search_token.take() {
                        token.cancel();
                    }
                    if let Some(token) = self.workspace_replace_token.take() {
                        token.cancel();
                    }
                    self.workspace_search_open = false;
                    self.workspace_replace_mode = false;
                    self.workspace_search_query.clear();
                    self.workspace_replace_value.clear();
                    self.workspace_search_results.clear();
                    self.workspace_search_error = None;
                }
                KeyCode::Up => {
                    self.workspace_search_selected =
                        self.workspace_search_selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    if !self.workspace_search_results.is_empty() {
                        self.workspace_search_selected = (self.workspace_search_selected + 1)
                            .min(self.workspace_search_results.len().saturating_sub(1));
                    }
                }
                KeyCode::Enter => {
                    if self.workspace_replace_mode {
                        self.start_workspace_replace();
                    } else {
                        self.open_workspace_search_result()?;
                    }
                }
                KeyCode::Backspace => {
                    let value = if self.workspace_replace_mode {
                        &mut self.workspace_replace_value
                    } else {
                        &mut self.workspace_search_query
                    };
                    if let Some((start, _)) = value.grapheme_indices(true).next_back() {
                        value.truncate(start);
                    }
                    if !self.workspace_replace_mode {
                        self.start_workspace_search();
                    }
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if self.workspace_replace_mode {
                        self.workspace_replace_value.push(character);
                    } else {
                        self.workspace_search_query.push(character);
                        self.start_workspace_search();
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.command_palette {
            return self.handle_command_palette(key);
        }
        if key.code == KeyCode::Char('p')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.open_command_palette();
            return Ok(false);
        }
        if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
            self.open_file_finder();
            return Ok(false);
        }
        if key.code == KeyCode::Char('f')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.open_workspace_search();
            return Ok(false);
        }
        if self.searching {
            if key.code == KeyCode::Char('f')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::ALT)
            {
                self.search_regex = !self.search_regex;
                self.refresh_search();
                return Ok(false);
            }
            if key.code == KeyCode::Char('r')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::ALT)
            {
                self.replace_all_matches()?;
                return Ok(false);
            }
            if key.code == KeyCode::Char('r')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.modifiers.contains(KeyModifiers::SHIFT)
            {
                self.replace_current_match()?;
                return Ok(false);
            }
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.search_query.clear();
                    self.search_matches.clear();
                }
                KeyCode::Backspace => {
                    if let Some((start, _)) = self.search_query.grapheme_indices(true).next_back() {
                        self.search_query.truncate(start);
                    }
                    self.refresh_search();
                }
                KeyCode::Enter => self.jump_to_next_match(),
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.search_query.push(ch);
                    self.refresh_search();
                }
                _ => {}
            }
            return Ok(false);
        }
        if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.switch_tab(if key.modifiers.contains(KeyModifiers::SHIFT) {
                -1
            } else {
                1
            });
            return Ok(false);
        }
        if key.code == KeyCode::PageDown && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.switch_tab(1);
            return Ok(false);
        }
        if key.code == KeyCode::PageUp && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.switch_tab(-1);
            return Ok(false);
        }
        if key.code == KeyCode::Char('w')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.close_tab(key.modifiers.contains(KeyModifiers::SHIFT));
            return Ok(false);
        }
        if key.code == KeyCode::Char('p')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            self.pin_current_tab();
            return Ok(false);
        }
        if key.code == KeyCode::F(9) {
            if key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                self.cancel_task()?;
                return Ok(false);
            }
            self.start_task(key.modifiers.contains(KeyModifiers::CONTROL))?;
            return Ok(false);
        }
        if key.code == KeyCode::Char('t')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.start_test_task()?;
            return Ok(false);
        }
        if key.code == KeyCode::F(4) {
            self.jump_to_first_problem()?;
            return Ok(false);
        }
        if key.code == KeyCode::Char('d')
            && key.modifiers.contains(KeyModifiers::ALT)
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            self.add_cursor_below();
            return Ok(false);
        }
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
            self.add_next_search_cursor();
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            if !self.readonly {
                let line = self.current_line();
                let column = self.current_column();
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                self.document.move_line(line, delta)?;
                let target_line = if delta < 0 {
                    line.saturating_sub(1)
                } else {
                    line.saturating_add(1)
                };
                self.cursor = self
                    .document
                    .line_column_to_byte(target_line, column)
                    .unwrap_or_else(|| self.document.line_to_byte(target_line).unwrap_or(0));
                self.selection = None;
                self.extra_cursors.clear();
            }
            return Ok(false);
        }
        if key.code == KeyCode::Char('t')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.send_selection_to_terminal(key.modifiers.contains(KeyModifiers::SHIFT))?;
            return Ok(false);
        }
        if key.code == KeyCode::Char('`') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.terminal_panel.is_fullscreen() {
                self.terminal_panel.toggle_fullscreen();
                self.terminal_panel.set_focus(false);
                self.notification = Some("editor focused".into());
            } else {
                self.toggle_terminal()?;
            }
            return Ok(false);
        }
        if key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.terminal_panel.sessions.is_empty() {
                self.toggle_terminal()?;
            }
            self.terminal_panel.toggle_fullscreen();
            self.terminal_panel.set_focus(true);
            self.notification = Some(if self.terminal_panel.is_fullscreen() {
                "terminal fullscreen".into()
            } else {
                "editor focused".into()
            });
            return Ok(false);
        }
        if key.code == KeyCode::Char('w')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            self.save_workspace_file()?;
            return Ok(false);
        }
        if key.code == KeyCode::Char('z')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.zen_mode = !self.zen_mode;
            self.terminal_panel.set_focus(false);
            self.notification = Some(if self.zen_mode {
                "zen mode enabled".into()
            } else {
                "zen mode disabled".into()
            });
            return Ok(false);
        }
        if key.code == KeyCode::Char('y')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.sticky_context = !self.sticky_context;
            self.notification = Some(
                if self.sticky_context {
                    "sticky context enabled"
                } else {
                    "sticky context disabled"
                }
                .into(),
            );
            return Ok(false);
        }
        if key.code == KeyCode::Char('d')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            self.toggle_editor_diff_mode();
            return Ok(false);
        }
        if key.code == KeyCode::Char('s')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.stage_editor_current_hunk()?;
            return Ok(false);
        }
        if key.code == KeyCode::Char('r')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.rollback_editor_current_hunk()?;
            return Ok(false);
        }
        if key.code == KeyCode::Char('\\')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.editor_split = true;
            self.editor_split_horizontal = !self.editor_split_horizontal;
            self.notification = Some(
                if self.editor_split_horizontal {
                    "horizontal editor split enabled"
                } else {
                    "vertical editor split enabled"
                }
                .into(),
            );
            return Ok(false);
        }
        if key.code == KeyCode::Char('\\')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.editor_split = !self.editor_split;
            self.notification = Some(
                if self.editor_split {
                    "editor split enabled"
                } else {
                    "editor split disabled"
                }
                .into(),
            );
            return Ok(false);
        }
        if key.code == KeyCode::Char('l')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.jump_to_terminal_link()?;
            return Ok(false);
        }
        if self.terminal_panel.is_focused()
            && key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.copy_terminal_selection();
            return Ok(false);
        }
        if self.route_terminal_key(key)? {
            return Ok(false);
        }
        if key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && key.code == KeyCode::Char('o')
        {
            self.open_workspace_root_input(key.modifiers.contains(KeyModifiers::SHIFT));
            return Ok(false);
        }
        if key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                let choice = match key.code {
                    KeyCode::Char('1') => Some(ConflictChoice::Ours),
                    KeyCode::Char('2') => Some(ConflictChoice::Theirs),
                    KeyCode::Char('3') => Some(ConflictChoice::Both),
                    KeyCode::Char('0') => Some(ConflictChoice::Neither),
                    _ => None,
                };
                if let Some(choice) = choice {
                    self.resolve_all_conflicts(choice)?;
                    return Ok(false);
                }
            }
            let choice = match key.code {
                KeyCode::Char('1') => Some(ConflictChoice::Ours),
                KeyCode::Char('2') => Some(ConflictChoice::Theirs),
                KeyCode::Char('3') => Some(ConflictChoice::Both),
                KeyCode::Char('0') => Some(ConflictChoice::Neither),
                _ => None,
            };
            if let Some(choice) = choice {
                self.resolve_conflict_at_cursor(choice)?;
                return Ok(false);
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            match key.code {
                KeyCode::Right => {
                    self.expand_selection();
                    return Ok(false);
                }
                KeyCode::Left => {
                    self.shrink_selection();
                    return Ok(false);
                }
                _ => {}
            }
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            if key.modifiers.contains(KeyModifiers::ALT) {
                if key.code == KeyCode::Char('(') {
                    self.surround_current_selection('(', ')')?;
                    return Ok(false);
                }
                if key.code == KeyCode::Char('"') {
                    self.surround_current_selection('"', '"')?;
                    return Ok(false);
                }
                if key.code == KeyCode::Backspace {
                    self.remove_surrounding_pair()?;
                    return Ok(false);
                }
                let style = match key.code {
                    KeyCode::Char('u') => Some(NamingStyle::Upper),
                    KeyCode::Char('l') => Some(NamingStyle::Lower),
                    KeyCode::Char('c') => Some(NamingStyle::Camel),
                    KeyCode::Char('p') => Some(NamingStyle::Pascal),
                    KeyCode::Char('s') => Some(NamingStyle::Snake),
                    KeyCode::Char('k') => Some(NamingStyle::Kebab),
                    _ => None,
                };
                if let Some(style) = style {
                    self.convert_current_identifier(style)?;
                    return Ok(false);
                }
            }
            if self.external_change {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('r'), modifiers)
                        if modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        self.reload_external()?;
                        return Ok(false);
                    }
                    (KeyCode::Char('s'), modifiers)
                        if modifiers.contains(KeyModifiers::CONTROL)
                            && modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        self.overwrite_external()?;
                        return Ok(false);
                    }
                    (KeyCode::Char('d'), modifiers)
                        if modifiers.contains(KeyModifiers::CONTROL)
                            && modifiers.contains(KeyModifiers::ALT) =>
                    {
                        self.compare_external()?;
                        return Ok(false);
                    }
                    _ => {}
                }
            }
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::Char('k') => {
                        self.delete_current_line()?;
                        return Ok(false);
                    }
                    KeyCode::Char('d') => {
                        self.duplicate_current_line()?;
                        return Ok(false);
                    }
                    KeyCode::Char('c') => {
                        self.copy_selection();
                        return Ok(false);
                    }
                    KeyCode::Char('x') => {
                        self.cut_selection()?;
                        return Ok(false);
                    }
                    KeyCode::Char('v') => {
                        self.paste_clipboard()?;
                        return Ok(false);
                    }
                    KeyCode::Char('r') => {
                        self.replace_current_match()?;
                        return Ok(false);
                    }
                    _ => {}
                }
            }
            if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::ALT) {
                self.word_wrap = !self.word_wrap;
                self.horizontal_scroll = 0;
                self.notification = Some(if self.word_wrap {
                    "soft wrap enabled".into()
                } else {
                    "horizontal scrolling enabled".into()
                });
                return Ok(false);
            }
            if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                self.move_by_word(key.code == KeyCode::Right);
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::ALT)
                && matches!(key.code, KeyCode::Char('[') | KeyCode::Char(']'))
            {
                if key.code == KeyCode::Char('[') {
                    self.set_all_folds(true);
                } else {
                    self.set_all_folds(false);
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('/') => {
                    self.toggle_comment_current_line()?;
                    return Ok(false);
                }
                KeyCode::Char('q') | KeyCode::Char('c') => return Ok(true),
                KeyCode::Char('s') => {
                    if let Err(error) = self.save() {
                        if matches!(error, AppError::ExternalChange(_)) {
                            self.external_change = true;
                            self.notification =
                                Some("save blocked: reload/compare/overwrite".into());
                        } else {
                            return Err(error);
                        }
                    }
                }
                KeyCode::Char('f') => {
                    self.searching = true;
                    self.search_query.clear();
                    self.search_matches.clear();
                }
                KeyCode::Char('g') => self.jump_to_next_symbol(),
                KeyCode::Char('[') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.toggle_fold_at_cursor();
                }
                KeyCode::Char('z') => {
                    let _ = self.document.undo();
                    self.cursor = self.cursor.min(self.document.text().len());
                }
                KeyCode::Char('y') => {
                    let _ = self.document.redo();
                }
                _ => {}
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => Ok(true),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_by_word(false);
                Ok(false)
            }
            KeyCode::Left => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.navigate_back();
                    return Ok(false);
                }
                let next = self.prev_boundary();
                self.update_selection(next, key.modifiers.contains(KeyModifiers::SHIFT));
                self.desired_column = self.current_column();
                Ok(false)
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_by_word(true);
                Ok(false)
            }
            KeyCode::Right => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.navigate_forward();
                    return Ok(false);
                }
                let next = self.next_boundary();
                self.update_selection(next, key.modifiers.contains(KeyModifiers::SHIFT));
                self.desired_column = self.current_column();
                Ok(false)
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_by_paragraph(-1);
                Ok(false)
            }
            KeyCode::Up => {
                self.move_vertical(-1);
                self.selection = None;
                self.extra_cursors.clear();
                Ok(false)
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_by_paragraph(1);
                Ok(false)
            }
            KeyCode::Down => {
                self.move_vertical(1);
                self.selection = None;
                self.extra_cursors.clear();
                Ok(false)
            }
            KeyCode::Home => {
                let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
                let start = self.document.line_to_byte(line).unwrap_or(0);
                let text = self.document.text();
                let end = self.document.line_to_byte(line + 1).unwrap_or(text.len());
                let line_text = &text[start..end];
                let first_non_space = line_text
                    .find(|character: char| !character.is_whitespace())
                    .unwrap_or(0);
                let target = if self.cursor == start + first_non_space {
                    start
                } else {
                    start + first_non_space
                };
                self.update_selection(target, key.modifiers.contains(KeyModifiers::SHIFT));
                self.desired_column = self.current_column();
                Ok(false)
            }
            KeyCode::End => {
                let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
                let start = self.document.line_to_byte(line).unwrap_or(0);
                let end = self
                    .document
                    .line_to_byte(line + 1)
                    .unwrap_or_else(|| self.document.text().len());
                let text = self.document.text();
                let target = end.saturating_sub(usize::from(
                    text.as_bytes().get(end.saturating_sub(1)) == Some(&b'\n'),
                ));
                let target = target.max(start);
                self.update_selection(target, key.modifiers.contains(KeyModifiers::SHIFT));
                self.desired_column = self.current_column();
                Ok(false)
            }
            KeyCode::Backspace => {
                if !self.readonly && (self.selection.is_some() || self.cursor > 0) {
                    if let Some(selection) = self.selection.take() {
                        let range = selection.range();
                        self.document.apply(&[Edit {
                            range: range.clone(),
                            insert: String::new(),
                        }])?;
                        self.cursor = range.start;
                    } else {
                        let start = self.prev_boundary();
                        self.document.apply(&[Edit {
                            range: start..self.cursor,
                            insert: String::new(),
                        }])?;
                        self.cursor = start;
                    }
                }
                Ok(false)
            }
            KeyCode::Enter => {
                if !self.readonly {
                    let text = self.document.text();
                    let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
                    let start = self.document.line_to_byte(line).unwrap_or(0);
                    let prefix = text[start..self.cursor]
                        .chars()
                        .take_while(|ch| ch.is_whitespace() && *ch != '\n')
                        .collect::<String>();
                    let extra = text[..self.cursor].ends_with('{');
                    let unit = if self.insert_spaces {
                        " ".repeat(self.tab_width as usize)
                    } else {
                        "\t".into()
                    };
                    let mut insertion = format!("\n{prefix}");
                    if extra {
                        insertion.push_str(&unit);
                    }
                    self.insert(&insertion)?;
                }
                Ok(false)
            }
            KeyCode::Tab => {
                if !self.readonly {
                    if self.advance_snippet(key.modifiers.contains(KeyModifiers::SHIFT)) {
                        return Ok(false);
                    }
                    if self.expand_current_snippet()? {
                        return Ok(false);
                    }
                    let indentation = if self.insert_spaces {
                        " ".repeat(self.tab_width as usize)
                    } else {
                        "\t".to_owned()
                    };
                    self.insert(&indentation)?;
                }
                Ok(false)
            }
            KeyCode::Char(ch) => {
                if !self.readonly {
                    let closing = matches!(ch, ')' | ']' | '}');
                    let pair = match ch {
                        '(' => Some(')'),
                        '[' => Some(']'),
                        '{' => Some('}'),
                        '"' => Some('"'),
                        '\'' => Some('\''),
                        _ => None,
                    };
                    let text = self.document.text();
                    if closing && text[self.cursor..].starts_with(ch) {
                        self.cursor += ch.len_utf8();
                    } else if let Some(close) = pair {
                        self.insert(&format!("{ch}{close}"))?;
                        self.cursor = self.cursor.saturating_sub(close.len_utf8());
                    } else {
                        self.insert(&ch.to_string())?;
                    }
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn advance_snippet(&mut self, backward: bool) -> bool {
        if self.snippet_stops.is_empty() {
            return false;
        }
        if backward {
            self.snippet_index = self.snippet_index.saturating_sub(1);
        } else if self.snippet_index + 1 < self.snippet_stops.len() {
            self.snippet_index += 1;
        } else {
            self.snippet_stops.clear();
            self.snippet_index = 0;
            return true;
        }
        let (start, end) = self.snippet_stops[self.snippet_index];
        self.cursor = end;
        self.selection = (start != end).then(|| Selection::new(start, end));
        true
    }

    fn convert_current_identifier(&mut self, style: NamingStyle) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        let text = self.document.text();
        let range = self.selection.map(Selection::range).unwrap_or_else(|| {
            let start = text[..self.cursor]
                .char_indices()
                .rev()
                .take_while(|(_, character)| character.is_alphanumeric() || *character == '_')
                .last()
                .map(|(offset, _)| offset)
                .unwrap_or(self.cursor);
            let end = text[self.cursor..]
                .char_indices()
                .take_while(|(_, character)| character.is_alphanumeric() || *character == '_')
                .last()
                .map(|(offset, character)| self.cursor + offset + character.len_utf8())
                .unwrap_or(self.cursor);
            start..end
        });
        if range.start == range.end {
            return Ok(());
        }
        let replacement = convert_identifier(&text[range.clone()], style);
        self.document.apply(&[Edit {
            range: range.clone(),
            insert: replacement.clone(),
        }])?;
        self.cursor = range.start + replacement.len();
        self.selection = None;
        self.extra_cursors.clear();
        Ok(())
    }

    fn selected_or_current_line(&self) -> Range<usize> {
        if let Some(selection) = self.selection {
            return selection.range();
        }
        let line = self.current_line();
        let start = self.document.line_to_byte(line).unwrap_or(0);
        let text = self.document.text();
        let end = self
            .document
            .line_to_byte(line + 1)
            .unwrap_or(text.len())
            .min(text.len());
        start
            ..end.saturating_sub(usize::from(
                text.as_bytes().get(end.saturating_sub(1)) == Some(&b'\n'),
            ))
    }

    fn smart_selection_range(&self) -> Range<usize> {
        let text = self.document.text();
        if let Some(selection) = self.selection {
            return selection.range();
        }
        let cursor = self.cursor.min(text.len());
        let mut start = cursor;
        let mut end = cursor;
        for (offset, character) in text[..cursor].char_indices().rev() {
            if character.is_alphanumeric() || character == '_' {
                start = offset;
            } else {
                break;
            }
        }
        for (offset, character) in text[cursor..].char_indices() {
            if character.is_alphanumeric() || character == '_' {
                end = cursor + offset + character.len_utf8();
            } else {
                break;
            }
        }
        if start != end {
            return start..end;
        }
        self.selected_or_current_line()
    }

    fn expand_selection(&mut self) {
        let had_selection = self.selection.is_some();
        let current = self.smart_selection_range();
        let text = self.document.text();
        if !had_selection {
            self.selection_history.push(None);
            self.selection = Some(Selection::new(current.start, current.end));
            self.cursor = current.end;
            return;
        }
        let mut next = current.clone();
        let pairs = [('(', ')'), ('[', ']'), ('{', '}')];
        if let (Some(left), Some(right)) = (
            text[..current.start].chars().next_back(),
            text[current.end..].chars().next(),
        ) {
            if pairs
                .iter()
                .any(|(open, close)| left == *open && right == *close)
            {
                next = current.start.saturating_sub(left.len_utf8())
                    ..(current.end + right.len_utf8()).min(text.len());
            }
        }
        if next == current {
            let line = self.document.byte_to_line(current.start).unwrap_or(0);
            let line_start = self.document.line_to_byte(line).unwrap_or(0);
            let line_end = self
                .document
                .line_to_byte(line + 1)
                .unwrap_or(text.len())
                .min(text.len());
            let trimmed_end = line_end.saturating_sub(usize::from(
                text.as_bytes().get(line_end.saturating_sub(1)) == Some(&b'\n'),
            ));
            if current.start > line_start || current.end < trimmed_end {
                next = line_start..trimmed_end;
            } else {
                next = 0..text.len();
            }
        }
        if next.start == current.start && next.end == current.end {
            return;
        }
        self.selection_history.push(self.selection);
        self.selection = Some(Selection::new(next.start, next.end));
        self.cursor = next.end;
    }

    fn shrink_selection(&mut self) {
        let Some(previous) = self.selection_history.pop() else {
            return;
        };
        let current = self.selection.map(Selection::range);
        self.selection = previous;
        self.cursor = self
            .selection
            .map(|selection| selection.active)
            .or_else(|| current.map(|range| range.start))
            .unwrap_or(self.cursor);
    }

    fn surround_current_selection(&mut self, open: char, close: char) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        let range = self.selected_or_current_line();
        let selected = self.document.text()[range.clone()].to_owned();
        let replacement = format!("{open}{selected}{close}");
        self.document.apply(&[Edit {
            range: range.clone(),
            insert: replacement,
        }])?;
        self.cursor = range.start + 1 + selected.len();
        self.selection = None;
        self.extra_cursors.clear();
        Ok(())
    }

    fn remove_surrounding_pair(&mut self) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        let text = self.document.text();
        let cursor = self.cursor.min(text.len());
        let pairs = [('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];
        let Some((open, close)) = pairs.into_iter().find(|(open, close)| {
            cursor > 0 && text[..cursor].ends_with(*open) && text[cursor..].starts_with(*close)
        }) else {
            return Ok(());
        };
        let start = text[..cursor].rfind(open).unwrap_or(cursor);
        let end = cursor + close.len_utf8();
        if start >= cursor {
            return Ok(());
        }
        self.document.apply(&[
            Edit {
                range: start..start + open.len_utf8(),
                insert: String::new(),
            },
            Edit {
                range: end..end + close.len_utf8(),
                insert: String::new(),
            },
        ])?;
        self.cursor = cursor.saturating_sub(open.len_utf8());
        Ok(())
    }

    fn expand_current_snippet(&mut self) -> Result<bool, AppError> {
        let text = self.document.text();
        let start = text[..self.cursor]
            .char_indices()
            .rev()
            .take_while(|(_, character)| character.is_ascii_alphanumeric() || *character == '_')
            .last()
            .map(|(offset, _)| offset)
            .unwrap_or(self.cursor);
        let trigger = &text[start..self.cursor];
        let body = match trigger {
            "for" => "for ${1:item} in ${2:items} {\n    $0\n}",
            "fn" => "fn ${1:name}(${2:args}) {\n    $0\n}",
            _ => return Ok(false),
        };
        let filename = self
            .path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        let expansion = expand_snippet(body, filename);
        self.document.apply(&[Edit {
            range: start..self.cursor,
            insert: expansion.text,
        }])?;
        self.snippet_stops = expansion
            .tabstops
            .into_iter()
            .map(|(begin, end)| (start + begin, start + end))
            .collect();
        self.snippet_index = 0;
        if let Some((begin, end)) = self.snippet_stops.first().copied() {
            self.cursor = end;
            self.selection = (begin != end).then(|| Selection::new(begin, end));
        } else {
            self.cursor = start + body.len();
        }
        Ok(true)
    }

    fn insert(&mut self, text: &str) -> Result<(), AppError> {
        if !self.extra_cursors.is_empty() && self.selection.is_none() {
            let mut positions = self.extra_cursors.clone();
            positions.push(self.cursor);
            positions.sort_unstable();
            positions.dedup();
            let primary = positions
                .iter()
                .position(|position| *position == self.cursor)
                .unwrap_or(0);
            self.document.insert_at_cursors(&positions, text)?;
            let new_positions = positions
                .iter()
                .enumerate()
                .map(|(index, position)| position + index * text.len())
                .collect::<Vec<_>>();
            self.cursor = new_positions[primary];
            self.extra_cursors = new_positions
                .into_iter()
                .enumerate()
                .filter_map(|(index, position)| (index != primary).then_some(position))
                .collect();
            self.desired_column = self.current_column();
            return Ok(());
        }
        let range = self
            .selection
            .take()
            .map(Selection::range)
            .unwrap_or(self.cursor..self.cursor);
        self.document.apply(&[Edit {
            range: range.clone(),
            insert: text.into(),
        }])?;
        self.cursor = range.start + text.len();
        self.desired_column = self.current_column();
        Ok(())
    }

    fn copy_selection(&mut self) {
        let text = self
            .selection
            .and_then(|selection| self.document.selected_text(selection).ok())
            .unwrap_or_default();
        if text.is_empty() {
            return;
        }
        self.clipboard = text.clone();
        self.clipboard_history.push(text);
    }

    fn cut_selection(&mut self) -> Result<(), AppError> {
        let Some(selection) = self.selection else {
            return Ok(());
        };
        self.clipboard = self.document.selected_text(selection)?;
        self.clipboard_history.push(self.clipboard.clone());
        let range = selection.range();
        self.document.apply(&[Edit {
            range: range.clone(),
            insert: String::new(),
        }])?;
        self.cursor = range.start;
        self.selection = None;
        Ok(())
    }

    fn paste_clipboard(&mut self) -> Result<(), AppError> {
        if !self.clipboard.is_empty() && !self.readonly {
            let clipboard = self.clipboard.clone();
            self.insert(&clipboard)?;
        }
        Ok(())
    }

    fn replace_current_match(&mut self) -> Result<(), AppError> {
        if self.search_matches.is_empty() || self.clipboard.is_empty() || self.readonly {
            return Ok(());
        }
        let Some(range) = self
            .search_matches
            .iter()
            .find(|range| range.start >= self.cursor)
            .cloned()
            .or_else(|| self.search_matches.first().cloned())
        else {
            return Ok(());
        };
        let replacement_len = self.clipboard.len();
        self.document.apply(&[Edit {
            range: range.clone(),
            insert: self.clipboard.clone(),
        }])?;
        self.cursor = range.start + replacement_len;
        self.refresh_search();
        self.notification = Some("replaced current match".into());
        Ok(())
    }

    fn replace_all_matches(&mut self) -> Result<(), AppError> {
        if self.search_query.is_empty() || self.clipboard.is_empty() || self.readonly {
            return Ok(());
        }
        let count = if self.search_regex {
            self.document
                .replace_all_regex(&self.search_query, &self.clipboard)?
        } else {
            self.document
                .replace_all_literal(&self.search_query, &self.clipboard)?
        };
        self.refresh_search();
        self.notification = Some(format!("replaced {count} matches"));
        Ok(())
    }

    fn current_line(&self) -> usize {
        self.document.byte_to_line(self.cursor).unwrap_or(0)
    }

    fn resolve_conflict_at_cursor(&mut self, choice: ConflictChoice) -> Result<(), AppError> {
        if self.readonly {
            self.notification = Some("cannot resolve conflict in read-only file".into());
            return Ok(());
        }
        let text = self.document.text();
        let Some(block) = find_conflict_blocks(&text)
            .into_iter()
            .find(|block| self.cursor >= block.range.start && self.cursor <= block.range.end)
        else {
            self.notification = Some("no conflict block at cursor".into());
            return Ok(());
        };
        let replacement = match choice {
            ConflictChoice::Ours => block.ours,
            ConflictChoice::Theirs => block.theirs,
            ConflictChoice::Both => format!("{}{}", block.ours, block.theirs),
            ConflictChoice::Neither => String::new(),
        };
        let block_start = block.range.start;
        self.document.apply(&[Edit {
            range: block.range,
            insert: replacement,
        }])?;
        self.cursor = block_start.min(self.document.text().len());
        self.selection = None;
        self.extra_cursors.clear();
        self.notification = Some(
            match choice {
                ConflictChoice::Ours => "conflict resolved: ours",
                ConflictChoice::Theirs => "conflict resolved: theirs",
                ConflictChoice::Both => "conflict resolved: both",
                ConflictChoice::Neither => "conflict block removed",
            }
            .into(),
        );
        Ok(())
    }

    fn resolve_all_conflicts(&mut self, choice: ConflictChoice) -> Result<(), AppError> {
        if self.readonly {
            self.notification = Some("cannot resolve conflict in read-only file".into());
            return Ok(());
        }
        let blocks = find_conflict_blocks(&self.document.text());
        if blocks.is_empty() {
            self.notification = Some("no conflict blocks found".into());
            return Ok(());
        }
        let edits = blocks
            .iter()
            .map(|block| Edit {
                range: block.range.clone(),
                insert: match choice {
                    ConflictChoice::Ours => block.ours.clone(),
                    ConflictChoice::Theirs => block.theirs.clone(),
                    ConflictChoice::Both => format!("{}{}", block.ours, block.theirs),
                    ConflictChoice::Neither => String::new(),
                },
            })
            .collect::<Vec<_>>();
        let cursor = blocks[0].range.start;
        self.document.apply(&edits)?;
        self.cursor = cursor.min(self.document.text().len());
        self.selection = None;
        self.extra_cursors.clear();
        self.notification = Some(format!(
            "resolved {} conflict block{}",
            blocks.len(),
            if blocks.len() == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    fn restore_conflict_state(&mut self) -> Result<(), AppError> {
        let Some(path) = self.path.clone() else {
            self.notification = Some("no conflict file is active".into());
            return Ok(());
        };
        let Some(snapshot) = self.conflict_snapshots.get(&path).cloned() else {
            self.notification = Some("no saved conflict state for this file".into());
            return Ok(());
        };
        let length = self.document.text().len();
        self.document.apply(&[Edit {
            range: 0..length,
            insert: snapshot.clone(),
        }])?;
        self.cursor = self.cursor.min(snapshot.len());
        self.notification = Some("conflict state restored; save to keep markers".into());
        Ok(())
    }

    fn delete_current_line(&mut self) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        let line = self.current_line();
        self.document.delete_line(line)?;
        self.cursor = self
            .document
            .line_to_byte(line.min(self.document.line_count().saturating_sub(1)))
            .unwrap_or(0);
        self.selection = None;
        self.extra_cursors.clear();
        Ok(())
    }

    fn duplicate_current_line(&mut self) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        let line = self.current_line();
        self.document.duplicate_line(line)?;
        self.selection = None;
        self.extra_cursors.clear();
        Ok(())
    }

    fn toggle_comment_current_line(&mut self) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        let text = self.document.text();
        let language = self
            .path
            .as_deref()
            .map(|path| mtide_syntax::detect_with_text(path, &text))
            .unwrap_or(mtide_syntax::Language::Rust);
        let marker = if matches!(language, mtide_syntax::Language::Python) {
            "#"
        } else {
            "//"
        };
        self.document
            .toggle_line_comment(self.current_line(), marker)?;
        self.selection = None;
        self.extra_cursors.clear();
        Ok(())
    }

    fn add_cursor_below(&mut self) {
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let Some(position) = self
            .document
            .line_column_to_byte(line + 1, self.current_column())
        else {
            return;
        };
        if position != self.cursor && !self.extra_cursors.contains(&position) {
            self.extra_cursors.push(position);
            self.extra_cursors.sort_unstable();
            if self.extra_cursors.len() > MAX_EXTRA_CURSORS {
                self.extra_cursors.truncate(MAX_EXTRA_CURSORS);
                self.notification = Some(format!(
                    "multi-cursor limit reached ({MAX_EXTRA_CURSORS}); extra cursors ignored"
                ));
            }
        }
    }

    fn add_next_search_cursor(&mut self) {
        let Some(range) = self
            .search_matches
            .iter()
            .find(|range| range.start > self.cursor)
            .cloned()
            .or_else(|| self.search_matches.first().cloned())
        else {
            self.notification = Some("no search matches for multi-cursor".into());
            return;
        };
        if range.start == self.cursor || self.extra_cursors.contains(&range.start) {
            self.notification = Some("all search matches already selected".into());
            return;
        }
        if self.extra_cursors.len() >= MAX_EXTRA_CURSORS {
            self.notification = Some(format!(
                "multi-cursor limit reached ({MAX_EXTRA_CURSORS}); extra cursors ignored"
            ));
            return;
        }
        self.extra_cursors.push(range.start);
        self.extra_cursors.sort_unstable();
        self.notification = Some(format!(
            "multi-cursor: {} active",
            self.extra_cursors.len() + 1
        ));
    }

    fn add_cursors_for_search_matches(&mut self) {
        if self.search_matches.is_empty() {
            self.notification = Some("no search matches for multi-cursor".into());
            return;
        }
        let mut positions = self
            .search_matches
            .iter()
            .map(|range| range.start)
            .filter(|position| *position != self.cursor)
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        let total = positions.len() + 1;
        let truncated = total > MAX_EXTRA_CURSORS + 1;
        positions.truncate(MAX_EXTRA_CURSORS);
        self.extra_cursors = positions;
        self.selection = None;
        self.notification = Some(if truncated {
            format!(
                "selected first {} of {} search matches (multi-cursor limit {})",
                self.extra_cursors.len() + 1,
                total,
                MAX_EXTRA_CURSORS + 1
            )
        } else {
            format!("selected {} search matches", self.extra_cursors.len() + 1)
        });
    }

    fn update_selection(&mut self, cursor: usize, extend: bool) {
        self.extra_cursors.clear();
        if extend {
            let anchor = self
                .selection
                .map(|selection| selection.anchor)
                .unwrap_or(self.cursor);
            self.selection = Some(Selection::new(anchor, cursor));
        } else {
            self.selection = None;
        }
        self.cursor = cursor;
    }

    fn move_by_word(&mut self, forward: bool) {
        let text = self.document.text();
        let target = if forward {
            let suffix = &text[self.cursor.min(text.len())..];
            let mut index = 0;
            let mut saw_word = false;
            for (offset, character) in suffix.char_indices() {
                if character.is_whitespace() {
                    if saw_word {
                        index = offset;
                        break;
                    }
                } else {
                    saw_word = true;
                    index = offset + character.len_utf8();
                }
            }
            self.cursor + index
        } else {
            let prefix = &text[..self.cursor.min(text.len())];
            let mut index = prefix.len();
            let mut saw_word = false;
            for (offset, character) in prefix.char_indices().rev() {
                if character.is_whitespace() {
                    if saw_word {
                        index = offset + character.len_utf8();
                        break;
                    }
                } else {
                    saw_word = true;
                    index = offset;
                }
            }
            index
        };
        self.update_selection(target.min(text.len()), false);
        self.desired_column = self.current_column();
    }

    fn move_by_paragraph(&mut self, direction: isize) {
        let text = self.document.text();
        let lines = text.split('\n').collect::<Vec<_>>();
        if lines.is_empty() {
            return;
        }
        let current = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let mut target = current;
        if direction < 0 {
            while target > 0 && !lines[target - 1].trim().is_empty() {
                target -= 1;
            }
            if target == current {
                while target > 0 && lines[target - 1].trim().is_empty() {
                    target -= 1;
                }
                while target > 0 && !lines[target - 1].trim().is_empty() {
                    target -= 1;
                }
            }
        } else {
            while target + 1 < lines.len() && !lines[target + 1].trim().is_empty() {
                target += 1;
            }
            while target + 1 < lines.len() && lines[target + 1].trim().is_empty() {
                target += 1;
            }
            if target + 1 < lines.len() {
                target += 1;
            }
        }
        let target_column = self
            .desired_column
            .min(lines[target].graphemes(true).count());
        if let Some(position) = self.document.line_column_to_byte(target, target_column) {
            self.cursor = position;
            self.selection = None;
        }
    }
    fn prev_boundary(&self) -> usize {
        let text = self.document.text();
        text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }
    fn next_boundary(&self) -> usize {
        let text = self.document.text();
        self.cursor
            + text[self.cursor..]
                .graphemes(true)
                .next()
                .map(str::len)
                .unwrap_or(0)
    }
    fn move_vertical(&mut self, delta: isize) {
        if self.word_wrap && self.wrap_width > 0 && self.move_visual_vertical(delta) {
            return;
        }
        if let Some(line) = self.document.byte_to_line(self.cursor) {
            let target = line as isize + delta;
            if target >= 0 {
                if let Some(position) = self
                    .document
                    .line_column_to_byte(target as usize, self.desired_column)
                {
                    self.cursor = position.min(self.document.text().len());
                }
            }
        }
    }

    fn move_visual_vertical(&mut self, delta: isize) -> bool {
        let text = self.document.text();
        let current_line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let line_start = self.document.line_to_byte(current_line).unwrap_or(0);
        let column =
            UnicodeSegmentation::graphemes(&text[line_start..self.cursor.min(text.len())], true)
                .count();
        let current_segment = column / self.wrap_width;
        let current_visual = text
            .split('\n')
            .take(current_line)
            .map(|line| {
                line.graphemes(true)
                    .count()
                    .div_ceil(self.wrap_width)
                    .max(1)
            })
            .sum::<usize>()
            + current_segment;
        let target_visual = current_visual as isize + delta;
        if target_visual < 0 {
            return false;
        }
        let mut visual = 0usize;
        for (line, contents) in text.split('\n').enumerate() {
            let count = contents.graphemes(true).count();
            let segments = count.div_ceil(self.wrap_width).max(1);
            if (visual..visual + segments).contains(&(target_visual as usize)) {
                let segment = target_visual as usize - visual;
                let target_column =
                    (segment * self.wrap_width + column % self.wrap_width).min(count);
                if let Some(position) = self.document.line_column_to_byte(line, target_column) {
                    self.cursor = position;
                    return true;
                }
            }
            visual += segments;
        }
        false
    }

    fn current_column(&self) -> usize {
        let text = self.document.text();
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let start = self.document.line_to_byte(line).unwrap_or(0);
        UnicodeSegmentation::graphemes(&text[start..self.cursor], true).count()
    }

    fn open_save_as(&mut self) {
        if self.readonly {
            self.notification = Some("buffer is read-only".into());
            return;
        }
        self.save_as_open = true;
        self.save_as_input.clear();
    }

    fn apply_save_as(&mut self) -> Result<(), AppError> {
        let input = self.save_as_input.trim();
        if input.is_empty() {
            self.notification = Some("save path is empty".into());
            return Ok(());
        }
        let target = PathBuf::from(input);
        let target = if target.is_absolute() {
            target
        } else {
            env::current_dir()?.join(target)
        };
        let previous_path = self.path.clone();
        let previous_baseline = self.baseline.clone();
        self.path = Some(target.clone());
        self.baseline = None;
        if let Err(error) = self.save() {
            self.path = previous_path;
            self.baseline = previous_baseline;
            return Err(error);
        }
        self.watcher = target
            .parent()
            .and_then(|parent| WorkspaceWatcher::new(parent).ok());
        self.git_repository = repository_for_path(&target);
        self.save_as_open = false;
        self.save_as_input.clear();
        self.notification = Some(format!("saved as {}", target.display()));
        Ok(())
    }

    fn conflict_marker_count(text: &str) -> usize {
        text.lines()
            .filter(|line| {
                let line = line.trim_start();
                line.starts_with("<<<<<<<")
                    || line.starts_with("=======")
                    || line.starts_with(">>>>>>>")
            })
            .count()
    }

    fn save(&mut self) -> Result<(), AppError> {
        if self.readonly {
            return Ok(());
        }
        if self.path.is_none() {
            self.open_save_as();
            return Ok(());
        }
        if let Some(path) = &self.path {
            if let Some(baseline) = &self.baseline {
                if changed_since(path, baseline)? {
                    return Err(AppError::ExternalChange(path.clone()));
                }
            }
            let mut text = self.document.text();
            let had_conflict_markers = Self::conflict_marker_count(&text) > 0;
            if self.trim_trailing_whitespace {
                text = text
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            if self.insert_final_newline && !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            let text = normalize_line_endings(&text, self.line_ending);
            let bytes =
                encode_text(&text, self.encoding).ok_or_else(|| AppError::Binary(path.clone()))?;
            atomic_write(path, &bytes)?;
            self.document.mark_saved();
            self.baseline = fingerprint(path).ok();
            if had_conflict_markers && Self::conflict_marker_count(&text) == 0 {
                if let Some(repository) = self.git_repository.clone() {
                    repository.stage(path)?;
                    self.notification = Some("saved and marked conflict resolved".into());
                    self.last_git_poll = Instant::now() - Duration::from_secs(1);
                }
            }
        }
        Ok(())
    }

    fn reload_external(&mut self) -> Result<(), AppError> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let bytes = fs::read(&path)?;
        let Some((encoding, text)) = decode_text(&bytes) else {
            return Err(AppError::Binary(path));
        };
        self.document = Document::new(DocId(1), &text);
        self.cursor = self.cursor.min(text.len());
        self.encoding = encoding;
        self.line_ending = mtide_workspace::detect_line_ending(&text);
        self.baseline = fingerprint(&path).ok();
        self.external_change = false;
        self.notification = Some("reloaded external file".into());
        Ok(())
    }

    fn overwrite_external(&mut self) -> Result<(), AppError> {
        if let Some(path) = &self.path {
            self.baseline = fingerprint(path).ok();
        }
        self.save()?;
        self.external_change = false;
        self.notification = Some("overwrote external file".into());
        Ok(())
    }

    fn compare_external(&mut self) -> Result<(), AppError> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let Some((_, external)) = decode_text(&fs::read(path)?) else {
            return Err(AppError::Binary(path.to_path_buf()));
        };
        let current = self.document.text();
        let differing_lines = current
            .lines()
            .zip(external.lines())
            .filter(|(left, right)| left != right)
            .count()
            + current.lines().count().abs_diff(external.lines().count());
        self.notification = Some(format!(
            "compare: {differing_lines} differing lines (Ctrl-R reload, Ctrl-Shift-S overwrite)"
        ));
        Ok(())
    }

    fn refresh_search(&mut self) {
        self.search_matches = if self.search_regex {
            self.document
                .find_regex(&self.search_query)
                .unwrap_or_default()
        } else {
            self.document.find_literal(&self.search_query)
        };
    }

    fn open_workspace_search(&mut self) {
        self.workspace_search_open = true;
        self.workspace_replace_mode = false;
        self.workspace_search_query.clear();
        self.workspace_search_results.clear();
        self.workspace_search_error = None;
        self.workspace_search_selected = 0;
    }

    fn start_workspace_search(&mut self) {
        if let Some(token) = self.workspace_search_token.take() {
            token.cancel();
        }
        self.workspace_search_results.clear();
        self.workspace_search_error = None;
        self.workspace_search_selected = 0;
        if self.workspace_search_query.is_empty() {
            self.workspace_search_job = None;
            return;
        }
        let mut roots = self.workspace_roots.clone();
        if roots.is_empty() {
            if let Some(parent) = self.path.as_deref().and_then(Path::parent) {
                roots.push(parent.to_path_buf());
            } else if let Ok(current) = env::current_dir() {
                roots.push(current);
            }
        }
        let query = self.workspace_search_query.clone();
        let options = SearchOptions {
            regex: self.workspace_search_regex,
            case_sensitive: self.workspace_search_case_sensitive,
            whole_word: self.workspace_search_whole_word,
            ..SearchOptions::default()
        };
        let token = CancellationToken::default();
        let worker_token = token.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result =
                mtide_search::search_roots_with_options(&roots, &query, &options, &worker_token);
            let message = result.as_ref().err().map(ToString::to_string);
            let matches = result.unwrap_or_default();
            let _ = sender.send((matches, message));
        });
        self.workspace_search_token = Some(token);
        self.workspace_search_job = Some(receiver);
    }

    fn poll_workspace_search(&mut self) {
        let Some(job) = self.workspace_search_job.as_ref() else {
            return;
        };
        let Ok((results, error)) = job.try_recv() else {
            return;
        };
        self.workspace_search_job = None;
        self.workspace_search_token = None;
        self.workspace_search_results = results;
        self.workspace_search_error = error;
        self.workspace_search_selected = self
            .workspace_search_selected
            .min(self.workspace_search_results.len().saturating_sub(1));
    }

    fn start_workspace_replace(&mut self) {
        if self.workspace_replace_job.is_some() {
            self.notification = Some("workspace replacement is already running".into());
            return;
        }
        if self.workspace_search_results.is_empty() {
            self.notification = Some("no search results to replace".into());
            return;
        }
        let mut paths = BTreeSet::new();
        for result in &self.workspace_search_results {
            paths.insert(result.path.clone());
        }
        let query = self.workspace_search_query.clone();
        let replacement = self.workspace_replace_value.clone();
        let options = SearchOptions {
            regex: self.workspace_search_regex,
            case_sensitive: self.workspace_search_case_sensitive,
            whole_word: self.workspace_search_whole_word,
            ..SearchOptions::default()
        };
        let token = CancellationToken::default();
        let worker_token = token.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut snapshots = Vec::new();
            let mut replaced = 0;
            let mut skipped = Vec::new();
            let mut error = None;
            for path in paths {
                if worker_token.is_cancelled() {
                    error = Some("workspace replacement cancelled".into());
                    break;
                }
                let before = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(_read_error) => {
                        skipped.push(path);
                        continue;
                    }
                };
                if fs::metadata(&path)
                    .map(|metadata| metadata.permissions().readonly())
                    .unwrap_or(false)
                {
                    skipped.push(path);
                    continue;
                }
                let Ok(text) = String::from_utf8(before.clone()) else {
                    skipped.push(path);
                    continue;
                };
                let (updated, count) =
                    match mtide_search::replace_text(&text, &query, &replacement, &options) {
                        Ok(result) => result,
                        Err(replace_error) => {
                            error = Some(replace_error.to_string());
                            break;
                        }
                    };
                if count == 0 {
                    continue;
                }
                snapshots.push((path.clone(), before));
                if let Err(write_error) = atomic_write(&path, updated.as_bytes()) {
                    error = Some(write_error.to_string());
                    break;
                }
                replaced += count;
            }
            if error.is_some() {
                for (path, bytes) in &snapshots {
                    let _ = atomic_write(path, bytes);
                }
                snapshots.clear();
                replaced = 0;
            }
            let _ = sender.send((snapshots, replaced, skipped, error));
        });
        self.workspace_replace_token = Some(token);
        self.workspace_replace_job = Some(receiver);
    }

    fn poll_workspace_replace(&mut self) {
        let Some(job) = self.workspace_replace_job.as_ref() else {
            return;
        };
        let Ok((snapshots, replaced, skipped, error)) = job.try_recv() else {
            return;
        };
        self.workspace_replace_job = None;
        self.workspace_replace_token = None;
        if let Some(error) = error {
            self.notification = Some(format!("workspace replacement failed: {error}"));
            return;
        }
        self.workspace_replace_snapshots = snapshots;
        self.notification = Some(if skipped.is_empty() {
            format!("replaced {replaced} matches; Ctrl-Z restores files")
        } else {
            format!(
                "replaced {replaced} matches; skipped {} file(s); Ctrl-Z restores files",
                skipped.len()
            )
        });
        self.start_workspace_search();
    }

    fn rollback_workspace_replace(&mut self) -> Result<(), AppError> {
        if self.workspace_replace_snapshots.is_empty() {
            self.notification = Some("no workspace replacement snapshot".into());
            return Ok(());
        }
        for (path, bytes) in &self.workspace_replace_snapshots {
            atomic_write(path, bytes)?;
        }
        self.workspace_replace_snapshots.clear();
        self.notification = Some("workspace replacement rolled back".into());
        self.start_workspace_search();
        Ok(())
    }

    fn open_workspace_search_result(&mut self) -> Result<(), AppError> {
        let Some(result) = self
            .workspace_search_results
            .get(self.workspace_search_selected)
            .cloned()
        else {
            return Ok(());
        };
        self.open_file_tab(&result.path, false)?;
        self.cursor = self
            .document
            .line_column_to_byte(result.line.saturating_sub(1), result.column)
            .unwrap_or(0);
        self.workspace_search_open = false;
        self.workspace_search_query.clear();
        self.workspace_search_results.clear();
        self.workspace_search_error = None;
        self.notification = Some(format!(
            "opened search result: {}:{}",
            result.path.display(),
            result.line
        ));
        Ok(())
    }

    fn jump_to_next_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.cursor = self
            .search_matches
            .iter()
            .find(|range| range.start > self.cursor)
            .or_else(|| self.search_matches.first())
            .map(|range| range.start)
            .unwrap_or(self.cursor);
    }

    fn current_location(&self) -> NavigationLocation {
        NavigationLocation {
            path: self.path.clone(),
            byte: self.cursor,
        }
    }

    fn toggle_fold_at_cursor(&mut self) {
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        let text = self.document.text();
        let language = self
            .path
            .as_deref()
            .map(|path| mtide_syntax::detect_with_text(path, &text))
            .unwrap_or(mtide_syntax::Language::Rust);
        if let Some(region) = mtide_syntax::fold_regions(&text, language)
            .into_iter()
            .find(|region| region.start_line == line)
        {
            if !self.folded_lines.insert(region.start_line) {
                self.folded_lines.remove(&region.start_line);
            }
        }
    }

    fn set_all_folds(&mut self, folded: bool) {
        if self.large_file_mode {
            self.folded_lines.clear();
            return;
        }
        let text = self.document.text();
        let language = self
            .path
            .as_deref()
            .map(|path| mtide_syntax::detect_with_text(path, &text))
            .unwrap_or(mtide_syntax::Language::Rust);
        if folded {
            self.folded_lines = mtide_syntax::fold_regions(&text, language)
                .into_iter()
                .map(|region| region.start_line)
                .collect();
        } else {
            self.folded_lines.clear();
        }
    }

    fn toggle_bookmark(&mut self) {
        let line = self.document.byte_to_line(self.cursor).unwrap_or(0);
        if !self.bookmarks.insert(line) {
            self.bookmarks.remove(&line);
            self.notification = Some(format!("bookmark removed: line {}", line + 1));
        } else {
            self.notification = Some(format!("bookmark added: line {}", line + 1));
        }
    }

    fn toggle_theme(&mut self) -> Result<(), AppError> {
        let current = self
            .theme_choices
            .iter()
            .position(|theme| theme.eq_ignore_ascii_case(&self.theme))
            .unwrap_or(0);
        self.theme = self.theme_choices[(current + 1) % self.theme_choices.len()].clone();
        self.theme_package = discover_theme_package(&self.theme);
        self.save_workspace_file()?;
        self.notification = Some(format!("theme switched to {}", self.theme));
        Ok(())
    }

    fn begin_theme_preview(&mut self) {
        let current = self
            .theme_choices
            .iter()
            .position(|theme| theme.eq_ignore_ascii_case(&self.theme))
            .unwrap_or(0);
        self.theme_preview =
            Some(self.theme_choices[(current + 1) % self.theme_choices.len()].clone());
        self.notification = Some("theme preview started; Enter applies, Esc cancels".into());
    }

    fn cycle_theme_preview(&mut self, forward: bool) {
        let Some(current_name) = self.theme_preview.clone() else {
            return;
        };
        let current = self
            .theme_choices
            .iter()
            .position(|theme| theme.eq_ignore_ascii_case(&current_name))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % self.theme_choices.len()
        } else {
            current
                .checked_sub(1)
                .unwrap_or(self.theme_choices.len().saturating_sub(1))
        };
        self.theme_preview = Some(self.theme_choices[next].clone());
    }

    fn apply_theme_preview(&mut self) -> Result<(), AppError> {
        let Some(theme) = self.theme_preview.take() else {
            return Ok(());
        };
        self.theme = theme.clone();
        self.theme_package = discover_theme_package(&theme);
        self.save_workspace_file()?;
        self.notification = Some(format!("theme applied: {theme}"));
        Ok(())
    }

    fn clear_bookmarks(&mut self) {
        self.bookmarks.clear();
        self.notification = Some("bookmarks cleared".into());
    }

    fn toggle_git_entry_stage(&mut self) -> Result<(), AppError> {
        let Some(entry) = self.git_entries.get(self.git_selected).cloned() else {
            return Ok(());
        };
        if matches!(entry.kind, GitChangeKind::Unmerged) {
            self.notification = Some("resolve conflict markers before staging this file".into());
            return Ok(());
        }
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        if entry.index_code != '.' && entry.index_code != ' ' {
            repository.unstage(&repository.root().join(&entry.path))?;
            self.notification = Some(format!("unstaged {}", entry.path.display()));
        } else {
            repository.stage(&repository.root().join(&entry.path))?;
            self.notification = Some(format!("staged {}", entry.path.display()));
        }
        self.last_git_poll = Instant::now() - Duration::from_secs(1);
        Ok(())
    }

    fn stage_all_git_entries(&mut self) -> Result<(), AppError> {
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        for entry in self.git_entries.iter().filter(|entry| {
            !matches!(entry.kind, GitChangeKind::Unmerged)
                && (entry.index_code == '.' || entry.index_code == ' ' || entry.index_code == '?')
        }) {
            repository.stage(&repository.root().join(&entry.path))?;
        }
        self.notification = Some("staged all changes".into());
        self.last_git_poll = Instant::now() - Duration::from_secs(1);
        Ok(())
    }

    fn unstage_all_git_entries(&mut self) -> Result<(), AppError> {
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        for entry in self
            .git_entries
            .iter()
            .filter(|entry| entry.index_code != '.' && entry.index_code != ' ')
        {
            repository.unstage(&repository.root().join(&entry.path))?;
        }
        self.notification = Some("unstaged all changes".into());
        self.last_git_poll = Instant::now() - Duration::from_secs(1);
        Ok(())
    }

    fn commit_git_changes(&mut self) -> Result<(), AppError> {
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return Ok(());
        };
        if self.git_commit_message.trim().is_empty() {
            self.notification = Some("commit message must not be empty".into());
            return Ok(());
        }
        let amend = self.git_commit_amend;
        match repository.commit_with_options(&self.git_commit_message, amend) {
            Ok(()) => {
                self.git_commit_open = false;
                self.git_commit_message.clear();
                self.git_commit_amend = false;
                self.git_commit_feedback.clear();
                self.notification = Some(
                    if amend {
                        "commit amended"
                    } else {
                        "commit created"
                    }
                    .into(),
                );
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => {
                self.git_commit_feedback = error.to_string();
                self.notification = Some(format!("commit failed: {error}"));
            }
        }
        Ok(())
    }

    fn apply_git_stash(&mut self, pop: bool) -> Result<(), AppError> {
        let Some(stash) = self.git_stashes.get(self.git_stash_selected).cloned() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        let result = if pop {
            repository.stash_pop(&stash.reference)
        } else {
            repository.stash_apply(&stash.reference)
        };
        match result {
            Ok(()) => {
                self.notification = Some(if pop {
                    format!("popped {}", stash.reference)
                } else {
                    format!("applied {}", stash.reference)
                });
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("stash operation failed: {error}")),
        }
        Ok(())
    }

    fn create_git_stash(&mut self) -> Result<(), AppError> {
        let Some(repository) = self.git_repository.clone() else {
            self.notification = Some("no Git repository detected".into());
            return Ok(());
        };
        match repository.stash_push("mtide stash", true) {
            Ok(()) => {
                self.notification = Some("stash created (including untracked files)".into());
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("stash create failed: {error}")),
        }
        Ok(())
    }

    fn drop_git_stash(&mut self) -> Result<(), AppError> {
        let Some(stash) = self.git_stashes.get(self.git_stash_selected).cloned() else {
            return Ok(());
        };
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        match repository.stash_drop(&stash.reference) {
            Ok(()) => {
                self.notification = Some(format!("dropped {}", stash.reference));
                self.last_git_poll = Instant::now() - Duration::from_secs(1);
            }
            Err(error) => self.notification = Some(format!("stash drop failed: {error}")),
        }
        Ok(())
    }

    fn checkout_git_branch(&mut self, name: &str, mode: GitCheckoutMode) -> Result<(), AppError> {
        let Some(branch) = self
            .git_branches
            .iter()
            .find(|branch| branch.name == name)
            .cloned()
        else {
            return Ok(());
        };
        if branch.current || branch.remote {
            return Ok(());
        }
        if !self.git_entries.is_empty() && mode == GitCheckoutMode::Prompt {
            self.git_checkout_pending = Some(branch.name);
            self.notification =
                Some("uncommitted changes: s stash and switch, c carry changes, Esc cancel".into());
            return Ok(());
        }
        let Some(repository) = self.git_repository.clone() else {
            return Ok(());
        };
        if mode == GitCheckoutMode::Stash {
            if let Err(error) = repository.stash_push("mtide branch switch", true) {
                self.notification = Some(format!("could not stash changes: {error}"));
                return Ok(());
            }
        }
        repository.checkout_branch(&branch.name)?;
        self.git_branch = Some(branch.name.clone());
        self.git_panel_open = false;
        self.git_branch_mode = false;
        self.last_git_poll = Instant::now() - Duration::from_secs(1);
        self.notification = Some(format!("checked out {}", branch.name));
        Ok(())
    }

    fn jump_to_next_symbol(&mut self) {
        let text = self.document.text();
        let language = self
            .path
            .as_deref()
            .map(|path| mtide_syntax::detect_with_text(path, &text))
            .unwrap_or(mtide_syntax::Language::Rust);
        let Some(symbol) = mtide_syntax::symbols(&text, language)
            .into_iter()
            .map(|symbol| {
                let line = symbol.line;
                (symbol, self.document.line_to_byte(line))
            })
            .find(|(_, byte)| byte.is_some_and(|byte| byte > self.cursor))
        else {
            return;
        };
        if let Some(byte) = symbol.1 {
            self.navigation.push(self.current_location());
            self.cursor = byte;
            self.desired_column = 0;
        }
    }

    fn navigate_back(&mut self) {
        if let Some(location) = self.navigation.back(self.current_location()) {
            if location.path == self.path {
                self.cursor = location.byte.min(self.document.text().len());
            }
        }
    }

    fn navigate_forward(&mut self) {
        if let Some(location) = self.navigation.forward(self.current_location()) {
            if location.path == self.path {
                self.cursor = location.byte.min(self.document.text().len());
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), AppError> {
        if self.save_all_on_quit && self.document.is_dirty() && self.path.is_some() {
            self.save()?;
        }
        self.persist_session()
    }

    fn persist_session(&self) -> Result<(), AppError> {
        let mut tabs = self.tabs.clone();
        if tabs.is_empty() {
            tabs.push(self.current_editor_tab());
        } else if let Some(tab) = tabs.get_mut(self.active_tab) {
            let current = self.current_editor_tab();
            *tab = current;
        }
        let session_tabs = tabs
            .iter()
            .map(Self::session_tab_from_editor)
            .collect::<Vec<_>>();
        save_session(
            &self.session_path,
            &SessionSnapshot {
                path: self.path.clone(),
                text: self.document.text(),
                cursor_line: self.document.byte_to_line(self.cursor).unwrap_or(0),
                cursor_column: self.current_column(),
                workspace_roots: self.workspace_roots.clone(),
                folded_lines: self.folded_lines.iter().copied().collect(),
                tabs: session_tabs,
                active_tab: self.active_tab,
                bookmarks: self.bookmarks.iter().copied().collect(),
                recent_files: self.recent_files.clone(),
            },
        )?;
        Ok(())
    }

    fn session_tab_from_editor(tab: &EditorTab) -> SessionTab {
        let document = Document::new(DocId(1), &tab.text);
        let cursor_line = document.byte_to_line(tab.cursor).unwrap_or(0);
        let line_start = document.line_to_byte(cursor_line).unwrap_or(0);
        let cursor_column = UnicodeSegmentation::graphemes(
            &tab.text[line_start..tab.cursor.min(tab.text.len())],
            true,
        )
        .count();
        SessionTab {
            path: tab.path.clone(),
            text: tab.text.clone(),
            cursor_line,
            cursor_column,
            folded_lines: tab.folded_lines.iter().copied().collect(),
            pinned: tab.pinned,
            preview: tab.preview,
            dirty: tab.dirty,
        }
    }
}

fn discover_theme_package(name: &str) -> Option<ThemePackage> {
    if matches!(name.to_ascii_lowercase().as_str(), "retro" | "modern") {
        return None;
    }
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let themes_dir = home.join(".config").join("mtide").join("themes");
    discover_theme_packages(&themes_dir)
        .into_iter()
        .find(|(package_name, _)| package_name.eq_ignore_ascii_case(name))
        .map(|(_, package)| package)
        .or_else(|| load_theme_package(&themes_dir.join(format!("{name}.json"))).ok())
}

fn discover_user_theme_packages() -> Vec<(String, ThemePackage)> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    discover_theme_packages(&home.join(".config").join("mtide").join("themes"))
}

fn notification_presentation(message: &str) -> (&'static str, ColorSlot) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("failed")
        || lower.contains("error")
        || lower.contains("cannot")
        || lower.contains("refused")
        || lower.contains("deleted")
    {
        ("✖", ColorSlot::Error)
    } else if lower.contains("started") || lower.contains("running") {
        ("…", ColorSlot::Focus)
    } else if lower.contains("warning") || lower.contains("external") {
        ("!", ColorSlot::Focus)
    } else {
        ("✔", ColorSlot::Success)
    }
}

fn terminal_color_slot(color: VtColor) -> ColorSlot {
    match color {
        VtColor::Indexed(index) => ColorSlot::Indexed(index),
        VtColor::Rgb(red, green, blue) => ColorSlot::Rgb(red, green, blue),
        _ => ColorSlot::Default,
    }
}

fn terminal_background_slot(color: VtColor) -> Option<ColorSlot> {
    match color {
        VtColor::Default => None,
        other => Some(terminal_color_slot(other)),
    }
}

fn horizontal_view(line: &str, offset: usize, width: usize) -> String {
    line.graphemes(true).skip(offset).take(width).collect()
}

fn diff_context_visible(diff: Option<&LineDiff>, index: usize, context: usize) -> bool {
    let Some(diff) = diff else {
        return true;
    };
    let start = index.saturating_sub(context);
    let end = (index + context + 1).min(diff.operations.len());
    diff.operations[start..end]
        .iter()
        .any(|operation| !matches!(operation, DiffOp::Equal(_)))
}

fn sticky_context_lines(
    text: &str,
    language: mtide_syntax::Language,
    current_line: usize,
    limit: usize,
) -> Vec<usize> {
    let mut lines = mtide_syntax::fold_regions(text, language)
        .into_iter()
        .filter(|region| region.start_line < current_line && current_line <= region.end_line)
        .map(|region| region.start_line)
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines
        .into_iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn diff_syntax_style(kind: mtide_syntax::TokenKind) -> ColorSlot {
    match kind {
        mtide_syntax::TokenKind::Keyword => ColorSlot::Focus,
        mtide_syntax::TokenKind::Comment => ColorSlot::Muted,
        mtide_syntax::TokenKind::String => ColorSlot::Success,
        mtide_syntax::TokenKind::Number => ColorSlot::Rgb(255, 180, 80),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_git_diff_line(
    frame: &mut CellBuffer,
    x: u16,
    y: u16,
    prefix: &str,
    line: &str,
    kind: GitDiffLineKind,
    selected: bool,
    language: mtide_syntax::Language,
) {
    let background = match kind {
        GitDiffLineKind::Added => Some(ColorSlot::Success),
        GitDiffLineKind::Removed => Some(ColorSlot::Error),
        GitDiffLineKind::Equal => None,
    };
    let mut text = String::with_capacity(prefix.len() + line.len());
    text.push_str(prefix);
    text.push_str(line);
    let spans = mtide_syntax::highlight(line, language);
    let mut offset = 0_u16;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true) {
        let Some(symbol) = grapheme.chars().next() else {
            continue;
        };
        let source_offset = offset.saturating_sub(prefix.chars().count() as u16) as usize;
        let style = if selected {
            ColorSlot::Focus
        } else if source_offset < line.len() {
            spans
                .iter()
                .find(|span| span.start <= source_offset && source_offset < span.end)
                .map(|span| diff_syntax_style(span.kind))
                .unwrap_or(ColorSlot::Default)
        } else {
            ColorSlot::Default
        };
        frame.set(
            x.saturating_add(offset),
            y,
            Cell {
                symbol,
                style,
                background,
            },
        );
        offset =
            offset.saturating_add(unicode_width::UnicodeWidthStr::width(grapheme).max(1) as u16);
    }
}

fn render_git_diff(diff: &LineDiff) -> GitDiffRendered {
    let mut lines = Vec::with_capacity(diff.operations.len());
    let mut line_hunks = Vec::with_capacity(diff.operations.len());
    let mut hunks = Vec::new();
    let mut active_hunk = None;
    let mut next_hunk = 0;
    for operation in &diff.operations {
        let (line, kind) = match operation {
            DiffOp::Equal(line) => {
                active_hunk = None;
                line_hunks.push(None);
                (line.clone(), GitDiffLineKind::Equal)
            }
            DiffOp::Insert(line) => {
                if active_hunk.is_none() {
                    active_hunk = Some(next_hunk);
                    hunks.push(next_hunk);
                    next_hunk += 1;
                }
                line_hunks.push(active_hunk);
                (line.clone(), GitDiffLineKind::Added)
            }
            DiffOp::Delete(line) => {
                if active_hunk.is_none() {
                    active_hunk = Some(next_hunk);
                    hunks.push(next_hunk);
                    next_hunk += 1;
                }
                line_hunks.push(active_hunk);
                (line.clone(), GitDiffLineKind::Removed)
            }
        };
        lines.push((line, kind));
    }
    (lines, line_hunks, hunks)
}

fn editor_diff_line_marks(diff: &LineDiff) -> Vec<GitDiffLineKind> {
    diff.operations
        .iter()
        .filter_map(|operation| match operation {
            DiffOp::Equal(_) => Some(GitDiffLineKind::Equal),
            DiffOp::Insert(_) => Some(GitDiffLineKind::Added),
            DiffOp::Delete(_) => None,
        })
        .collect()
}

fn render_git_split(
    diff: &LineDiff,
    line_hunks: &[Option<usize>],
) -> Vec<(String, String, GitDiffLineKind, Option<usize>)> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < diff.operations.len() {
        match &diff.operations[index] {
            DiffOp::Equal(line) => {
                rows.push((
                    line.clone(),
                    line.clone(),
                    GitDiffLineKind::Equal,
                    line_hunks.get(index).copied().flatten(),
                ));
                index += 1;
            }
            DiffOp::Delete(_) | DiffOp::Insert(_) => {
                let start = index;
                let mut old = Vec::new();
                let mut new = Vec::new();
                while index < diff.operations.len()
                    && !matches!(diff.operations[index], DiffOp::Equal(_))
                {
                    match &diff.operations[index] {
                        DiffOp::Delete(line) => old.push(line.clone()),
                        DiffOp::Insert(line) => new.push(line.clone()),
                        DiffOp::Equal(_) => unreachable!(),
                    }
                    index += 1;
                }
                let hunk = line_hunks.get(start).copied().flatten();
                for row in 0..old.len().max(new.len()) {
                    rows.push((
                        old.get(row).cloned().unwrap_or_default(),
                        new.get(row).cloned().unwrap_or_default(),
                        if old.get(row).is_some() {
                            GitDiffLineKind::Removed
                        } else {
                            GitDiffLineKind::Added
                        },
                        hunk,
                    ));
                }
            }
        }
    }
    rows
}

#[derive(Clone, Debug)]
struct ConflictBlock {
    range: Range<usize>,
    ours: String,
    theirs: String,
}

fn find_conflict_blocks(text: &str) -> Vec<ConflictBlock> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        lines.push((offset, end, line));
        offset = end;
    }
    let mut blocks = Vec::new();
    let mut start = None;
    let mut separator = None;
    for (index, (_, _, line)) in lines.iter().enumerate() {
        let marker = line.trim_end_matches(&['\r', '\n'][..]);
        if start.is_none() && marker.trim_start().starts_with("<<<<<<<") {
            start = Some(index);
            continue;
        }
        if start.is_some() && separator.is_none() && marker.trim_start().starts_with("=======") {
            separator = Some(index);
            continue;
        }
        if let (Some(start_line), Some(separator_line)) = (start, separator) {
            if marker.trim_start().starts_with(">>>>>>>") {
                let ours_start = lines[start_line].1;
                let theirs_start = lines[separator_line].1;
                blocks.push(ConflictBlock {
                    range: lines[start_line].0..lines[index].1,
                    ours: text[ours_start..lines[separator_line].0].to_owned(),
                    theirs: text[theirs_start..lines[index].0].to_owned(),
                });
                start = None;
                separator = None;
            }
        }
    }
    blocks
}

fn default_commands() -> CommandRegistry {
    let mut commands = CommandRegistry::default();
    for (id, title, shortcut) in [
        ("help.keybindings", "Show keyboard shortcuts", "F1"),
        ("editor.save", "Save file", "Ctrl-S"),
        ("editor.saveAs", "Save file as...", ""),
        ("editor.nextTab", "Next tab", "Ctrl-Tab"),
        ("editor.previousTab", "Previous tab", "Ctrl-Shift-Tab"),
        ("editor.closeTab", "Close tab", "Ctrl-W"),
        ("editor.pinTab", "Pin current tab", "Ctrl-Alt-Shift-P"),
        ("editor.zen", "Toggle Zen mode", "Ctrl-Alt-Z"),
        ("editor.split", "Toggle editor split", "Ctrl-\\"),
        (
            "editor.stickyContext",
            "Toggle sticky editor context",
            "Ctrl-Alt-Y",
        ),
        (
            "editor.splitHorizontal",
            "Toggle horizontal/vertical split",
            "Ctrl-Shift-\\",
        ),
        ("ui.toggleTheme", "Toggle Retro/Modern theme", "Ctrl-Alt-M"),
        ("ui.theme.preview", "Preview next theme", ""),
        ("ui.keymap.vim", "Use Vim keymap preset", ""),
        ("ui.keymap.emacs", "Use Emacs keymap preset", ""),
        ("ui.keymap.vscode", "Use VSCode keymap preset", ""),
        ("ui.keymap.borland", "Use Borland keymap preset", ""),
        (
            "ui.keymap.importVimrc",
            "Import common mappings from ~/.vimrc",
            "",
        ),
        ("navigation.quickOpen", "Quick open file", "Ctrl-P"),
        ("navigation.recentFiles", "Recent files", "Ctrl-E"),
        ("search.workspace", "Search workspace", "Ctrl-Shift-F"),
        ("selection.expand", "Expand selection", "Ctrl-Alt-Right"),
        ("selection.shrink", "Shrink selection", "Ctrl-Alt-Left"),
        ("terminal.toggle", "Toggle terminal", "Ctrl-`"),
        ("terminal.new", "New terminal session", "Ctrl-Shift-N"),
        ("terminal.rename", "Rename terminal session", "Ctrl-Shift-R"),
        ("terminal.next", "Next terminal session", "Ctrl-PageDown"),
        (
            "terminal.previous",
            "Previous terminal session",
            "Ctrl-PageUp",
        ),
        ("terminal.close", "Close terminal session", "Ctrl-Shift-X"),
        ("terminal.link", "Open terminal link", "Ctrl-Shift-L"),
        ("task.build", "Build project", "F9"),
        ("task.run", "Run project", "Ctrl-F9"),
        ("task.test", "Run project tests", "Ctrl-Shift-T"),
        ("task.cancel", "Cancel task", "Ctrl-Shift-F9"),
        ("git.cancel", "Cancel running Git operation", "Ctrl-Shift-G"),
        ("task.results", "Task results and problems", "F6"),
        ("task.output", "Task output", "Ctrl-Shift-O"),
        ("navigation.toggleBookmark", "Toggle bookmark", "F11"),
        ("navigation.clearBookmarks", "Clear bookmarks", "Ctrl-F11"),
        ("git.panel", "Git changes", "F7"),
        (
            "git.diffEditor",
            "Toggle editable Git diff decorations",
            "Ctrl-Alt-Shift-D",
        ),
        (
            "git.diffStageHunk",
            "Stage current editor diff hunk",
            "Ctrl-Alt-S",
        ),
        (
            "git.diffRollbackHunk",
            "Rollback current editor diff hunk",
            "Ctrl-Alt-R",
        ),
        ("git.diffOriginal", "View Git original file", ""),
        ("git.commit", "Commit staged changes", ""),
        (
            "git.conflict.ours",
            "Resolve conflict: take ours",
            "Ctrl-Alt-1",
        ),
        (
            "git.conflict.theirs",
            "Resolve conflict: take theirs",
            "Ctrl-Alt-2",
        ),
        (
            "git.conflict.both",
            "Resolve conflict: take both",
            "Ctrl-Alt-3",
        ),
        (
            "git.conflict.neither",
            "Resolve conflict: take neither",
            "Ctrl-Alt-0",
        ),
        (
            "git.conflict.allOurs",
            "Resolve all conflicts: take ours",
            "Ctrl-Alt-Shift-1",
        ),
        (
            "git.conflict.allTheirs",
            "Resolve all conflicts: take theirs",
            "Ctrl-Alt-Shift-2",
        ),
        (
            "git.conflict.allBoth",
            "Resolve all conflicts: take both",
            "Ctrl-Alt-Shift-3",
        ),
        (
            "git.conflict.allNeither",
            "Resolve all conflicts: take neither",
            "Ctrl-Alt-Shift-0",
        ),
        (
            "git.conflict.restore",
            "Restore original conflict markers",
            "",
        ),
        ("workspace.addRoot", "Add workspace root", "Ctrl-Alt-O"),
        (
            "workspace.removeRoot",
            "Remove workspace root",
            "Ctrl-Alt-Shift-O",
        ),
        (
            "workspace.exportDiagnostics",
            "Export redacted diagnostics bundle",
            "",
        ),
    ] {
        commands.register(
            id,
            title,
            (!shortcut.is_empty()).then(|| shortcut.to_string()),
        );
    }
    commands
}

fn default_task(path: Option<&Path>, run: bool) -> Option<TaskSpec> {
    let start = path
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| env::current_dir().ok())?;
    let root = find_build_root(&start)?;
    if let Some(task) = configured_task(&root, run) {
        return Some(task);
    }
    let (program, args, label) = if root.join("Cargo.toml").is_file() {
        if run {
            ("cargo", vec!["run".into()], "cargo run")
        } else {
            ("cargo", vec!["check".into()], "cargo check")
        }
    } else if root.join("go.mod").is_file() {
        if run {
            ("go", vec!["run".into(), ".".into()], "go run")
        } else {
            ("go", vec!["build".into(), "./...".into()], "go build")
        }
    } else if root.join("package.json").is_file() {
        if run {
            ("npm", vec!["run".into(), "start".into()], "npm start")
        } else {
            ("npm", vec!["run".into(), "build".into()], "npm build")
        }
    } else if root.join("pyproject.toml").is_file()
        || root.join("setup.py").is_file()
        || root.join("requirements.txt").is_file()
    {
        if run {
            if let Some(file) = path.filter(|file| {
                file.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
            }) {
                (
                    "python",
                    vec![file.to_string_lossy().into_owned()],
                    "python run",
                )
            } else {
                ("python", vec!["-m".into(), "pytest".into()], "pytest")
            }
        } else {
            ("python", vec!["-m".into(), "pytest".into()], "pytest")
        }
    } else if root.join("Makefile").is_file() {
        if run {
            ("make", vec!["run".into()], "make run")
        } else {
            ("make", Vec::new(), "make")
        }
    } else {
        return None;
    };
    Some(TaskSpec {
        label: label.into(),
        program: program.into(),
        args,
        cwd: Some(root),
        environment: std::collections::BTreeMap::new(),
    })
}

fn default_test_task(path: Option<&Path>) -> Option<TaskSpec> {
    let start = path
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| env::current_dir().ok())?;
    let root = find_build_root(&start)?;
    let (program, args, label) = if root.join("Cargo.toml").is_file() {
        ("cargo", vec!["test".into()], "cargo test")
    } else if root.join("go.mod").is_file() {
        ("go", vec!["test".into(), "./...".into()], "go test")
    } else if root.join("package.json").is_file() {
        ("npm", vec!["test".into()], "npm test")
    } else if root.join("pyproject.toml").is_file()
        || root.join("setup.py").is_file()
        || root.join("requirements.txt").is_file()
    {
        ("python", vec!["-m".into(), "pytest".into()], "pytest")
    } else if root.join("Makefile").is_file() {
        ("make", vec!["test".into()], "make test")
    } else {
        return None;
    };
    Some(TaskSpec {
        label: label.into(),
        program: program.into(),
        args,
        cwd: Some(root),
        environment: std::collections::BTreeMap::new(),
    })
}

fn configured_task(root: &Path, run: bool) -> Option<TaskSpec> {
    configured_task_plan(root, if run { "run" } else { "build" })?.pop()
}

fn configured_task_plan(root: &Path, name: &str) -> Option<Vec<TaskSpec>> {
    let config_path = [root.join(".mtide/tasks.toml"), root.join("tasks.toml")]
        .into_iter()
        .find(|path| path.is_file())?;
    let source = fs::read_to_string(config_path).ok()?;
    let document = toml::from_str::<toml::Table>(&source).ok()?;
    let mut output = Vec::new();
    visit_configured_task(root, &document, name, &mut Vec::new(), &mut output)?;
    Some(output)
}

fn visit_configured_task(
    root: &Path,
    document: &toml::Table,
    name: &str,
    visiting: &mut Vec<String>,
    output: &mut Vec<TaskSpec>,
) -> Option<()> {
    if visiting.iter().any(|item| item == name) {
        return None;
    }
    visiting.push(name.to_owned());
    let value = document
        .get(name)
        .or_else(|| document.get("tasks").and_then(|tasks| tasks.get(name)))
        .or_else(|| {
            document.get("task").and_then(|tasks| {
                tasks
                    .as_array()?
                    .iter()
                    .find(|task| task.get("name").and_then(toml::Value::as_str) == Some(name))
            })
        })?;
    let table = value.as_table()?;
    let dependencies = table
        .get("depends_on")
        .or_else(|| table.get("dependsOn"))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for dependency in dependencies {
        visit_configured_task(root, document, &dependency, visiting, output)?;
    }
    let program = table
        .get("program")
        .or_else(|| table.get("command"))
        .and_then(toml::Value::as_str)?
        .to_owned();
    let args = table
        .get("args")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cwd = table
        .get("cwd")
        .and_then(toml::Value::as_str)
        .map(|cwd| {
            let path = PathBuf::from(cwd);
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .or_else(|| Some(root.to_path_buf()));
    let environment = table
        .get("env")
        .and_then(toml::Value::as_table)
        .map(|values| {
            values
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    output.push(TaskSpec {
        label: table
            .get("label")
            .or_else(|| table.get("name"))
            .and_then(toml::Value::as_str)
            .unwrap_or(name)
            .to_owned(),
        program,
        args,
        cwd,
        environment,
    });
    visiting.pop();
    Some(())
}

fn repository_for_path(path: &Path) -> Option<Repository> {
    let directory = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or_else(|| Path::new("."))
    };
    Repository::discover(directory).ok()
}

fn find_build_root(start: &Path) -> Option<PathBuf> {
    let mut directory = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if [
            "Cargo.toml",
            "go.mod",
            "package.json",
            "pyproject.toml",
            "setup.py",
            "requirements.txt",
            "Makefile",
            ".mtide/tasks.toml",
            "tasks.toml",
        ]
        .iter()
        .any(|name| directory.join(name).is_file())
        {
            return Some(directory);
        }
        if !directory.pop() {
            return None;
        }
    }
}

fn key_stroke(key: KeyEvent) -> Option<KeyStroke> {
    let stroke_key = match key.code {
        KeyCode::Char(ch) => ch,
        KeyCode::Enter => '\n',
        KeyCode::Tab => '\t',
        KeyCode::Backspace => '\u{8}',
        KeyCode::Esc => '\u{1b}',
        KeyCode::Left => '←',
        KeyCode::Right => '→',
        KeyCode::Up => '↑',
        KeyCode::Down => '↓',
        _ => return None,
    };
    Some(KeyStroke {
        key: stroke_key,
        control: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    })
}

fn parse_force_key(value: &str) -> Option<KeyStroke> {
    let mut control = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None;
    for part in value.trim().to_ascii_lowercase().split('-') {
        match part {
            "ctrl" | "control" => control = true,
            "alt" | "meta" => alt = true,
            "shift" => shift = true,
            "backtick" => key = Some('`'),
            "space" => key = Some(' '),
            "enter" => key = Some('\n'),
            "tab" => key = Some('\t'),
            "esc" | "escape" => key = Some('\u{1b}'),
            value if value.chars().count() == 1 => key = value.chars().next(),
            _ => return None,
        }
    }
    key.map(|key| KeyStroke {
        key,
        control,
        alt,
        shift,
    })
}

fn terminal_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let mut bytes = match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let lower = ch.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    vec![lower as u8 - b'a' + 1]
                } else if ch == ' ' {
                    vec![0]
                } else {
                    return None;
                }
            } else {
                let mut encoded = [0; 4];
                ch.encode_utf8(&mut encoded).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    };
    if key.modifiers.contains(KeyModifiers::ALT)
        && !matches!(
            key.code,
            KeyCode::Esc
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Delete
        )
    {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn terminal_mouse_bytes(kind: MouseEventKind, column: usize, row: usize) -> Option<Vec<u8>> {
    let (button, release) = match kind {
        MouseEventKind::Down(MouseButton::Left) => (0, false),
        MouseEventKind::Down(MouseButton::Middle) => (1, false),
        MouseEventKind::Down(MouseButton::Right) => (2, false),
        MouseEventKind::Up(_) => (3, true),
        MouseEventKind::Drag(MouseButton::Left) => (32, false),
        MouseEventKind::Drag(MouseButton::Middle) => (33, false),
        MouseEventKind::Drag(MouseButton::Right) => (34, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::Moved => (35, false),
        _ => return None,
    };
    let suffix = if release { 'm' } else { 'M' };
    Some(format!("\x1b[<{button};{column};{row}{suffix}").into_bytes())
}

fn main() -> Result<(), AppError> {
    install_panic_hook();
    if env::args().any(|argument| argument == "--doctor") {
        doctor();
        return Ok(());
    }
    let options = parse_args(env::args().skip(1));
    let workspace = options
        .workspace_file
        .as_deref()
        .map(WorkspaceFile::load)
        .transpose()?;
    // `run` owns the terminal until the user exits, which is the `--wait` contract.
    let _wait_for_exit = options.wait;
    let stdin_text = if options.path.is_none() && options.stdin {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text)?;
        Some(text)
    } else {
        None
    };
    let session_path = env::current_dir()?.join(".mtide").join("session.json");
    let interrupted = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&interrupted)).map_err(io::Error::other)?;
    let mut app = App::open_with_cancel(
        options.path,
        options.readonly,
        stdin_text,
        session_path,
        options.initial_position,
        options.theme_override,
        &interrupted,
    )?;
    app.workspace_file = options.workspace_file.clone();
    if let (Some(workspace), Some(workspace_path)) = (&workspace, options.workspace_file.as_deref())
    {
        for root in workspace.resolve_roots(workspace_path) {
            if !app.workspace_roots.contains(&root) {
                app.workspace_roots.push(root);
            }
        }
        app.sidebar_width = workspace
            .layout
            .get("sidebar.width")
            .and_then(|value| value.parse::<u16>().ok());
        if let Some(theme) = workspace.layout.get("theme") {
            app.theme = theme.clone();
            app.theme_package = discover_theme_package(&app.theme);
        }
        if let Some(keymap) = workspace.layout.get("keymap") {
            app.keymap_preset = keymap.clone();
            app.keymap = Keymap::preset(keymap);
        }
        if let Some(path) = workspace.layout.get("keymap.vimrc") {
            let path = PathBuf::from(path);
            if let Ok((_, _)) = app.keymap.import_vimrc(&path) {
                app.keymap_import_path = Some(path);
            }
        }
        app.editor_split = workspace
            .layout
            .get("editor.split")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        app.editor_split_horizontal = workspace
            .layout
            .get("editor.split.direction")
            .is_some_and(|value| value.eq_ignore_ascii_case("horizontal"));
        app.notification = Some(format!("workspace loaded: {} roots", workspace.roots.len()));
    }
    app.workspace_roots = options
        .additional_paths
        .iter()
        .filter_map(|path| {
            path.parent()
                .map(Path::to_path_buf)
                .or_else(|| Some(path.clone()))
        })
        .fold(app.workspace_roots, |mut roots, root| {
            if !roots.contains(&root) {
                roots.push(root);
            }
            roots
        });
    for path in &options.additional_paths {
        if path.is_file() {
            app.open_file_tab(path, false)?;
        }
    }
    if options.safe_mode {
        let defaults = mtide_workspace::Config::default();
        app.word_wrap = defaults.word_wrap;
        app.theme = defaults.theme;
        app.theme_package = None;
        app.keymap_preset = defaults.keymap;
        app.keymap = Keymap::preset(&app.keymap_preset);
        app.tab_width = defaults.tab_width;
        app.insert_spaces = defaults.insert_spaces;
        app.terminal_scrollback = defaults.terminal_scrollback;
        app.autosave_interval = Duration::from_secs(defaults.autosave_interval_secs);
        app.save_all_on_quit = defaults.save_all_on_quit;
        let force_keys = defaults
            .terminal_force_keys
            .iter()
            .filter_map(|key| parse_force_key(key))
            .collect::<Vec<_>>();
        app.terminal_panel.key_policy.set_force_keys(force_keys);
        app.config_warning = false;
        app.notification = Some("safe mode: user/project settings disabled".into());
    }
    app.run(interrupted)
}

#[derive(Debug, Default, Eq, PartialEq)]
struct CliOptions {
    path: Option<PathBuf>,
    initial_position: Option<(usize, usize)>,
    readonly: bool,
    stdin: bool,
    wait: bool,
    theme_override: Option<String>,
    additional_paths: Vec<PathBuf>,
    safe_mode: bool,
    no_lsp: bool,
    no_plugins: bool,
    log_level: Option<String>,
    workspace_file: Option<PathBuf>,
}

fn file_icon(path: Option<&Path>) -> &'static str {
    let Some(path) = path else {
        return "?";
    };
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "R",
        "toml" | "yaml" | "yml" | "json" => "C",
        "md" | "markdown" => "M",
        "js" | "jsx" | "ts" | "tsx" => "J",
        "py" => "P",
        "go" => "G",
        "sh" | "bash" | "zsh" => "S",
        "css" | "scss" | "html" => "W",
        _ => "F",
    }
}

fn parse_args<I>(arguments: I) -> CliOptions
where
    I: IntoIterator<Item = String>,
{
    let mut options = CliOptions::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--readonly" => options.readonly = true,
            "--wait" => options.wait = true,
            "--theme" => options.theme_override = arguments.next(),
            "--safe-mode" => options.safe_mode = true,
            "--no-lsp" => options.no_lsp = true,
            "--no-plugins" => options.no_plugins = true,
            "--log-level" => options.log_level = arguments.next(),
            "--workspace" => options.workspace_file = arguments.next().map(PathBuf::from),
            "-" => options.stdin = true,
            value if !value.starts_with('-') => {
                let (parsed_path, position) = parse_location(value);
                let path = PathBuf::from(parsed_path);
                if options.path.is_none() {
                    options.path = Some(path);
                    options.initial_position = position;
                } else {
                    options.additional_paths.push(path);
                }
            }
            _ => {}
        }
    }
    options
}

fn find_project_config(path: &Path) -> Option<PathBuf> {
    let mut directory = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        let candidate = directory.join(".mtide").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !directory.pop() {
            return None;
        }
    }
}

fn find_editorconfig(path: &Path) -> Option<PathBuf> {
    let mut directory = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        let candidate = directory.join(".editorconfig");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !directory.pop() {
            return None;
        }
    }
}

fn parse_location(value: &str) -> (&str, Option<(usize, usize)>) {
    let mut parts = value.rsplitn(3, ':');
    let Some(last) = parts.next() else {
        return (value, None);
    };
    let Some(previous) = parts.next() else {
        return (value, None);
    };
    let Some(path) = parts.next() else {
        return (value, None);
    };
    let Ok(column) = last.parse::<usize>() else {
        return (value, None);
    };
    let Ok(line) = previous.parse::<usize>() else {
        return (value, None);
    };
    (path, Some((line, column)))
}

fn doctor() {
    use std::io::IsTerminal;
    let stdin_tty = io::stdin().is_terminal();
    let stdout_tty = io::stdout().is_terminal();
    let size = terminal::size()
        .map(|(width, height)| format!("{width}x{height}"))
        .unwrap_or_else(|_| "unavailable".into());
    let git = Command::new("git")
        .arg("--version")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|_| "unavailable".into());
    println!("mtide doctor");
    println!("stdin TTY: {stdin_tty}");
    println!("stdout TTY: {stdout_tty}");
    println!("terminal size: {size}");
    println!("color disabled: {}", env::var_os("NO_COLOR").is_some());
    let capabilities = TerminalCapabilities::detect();
    let color = match capabilities.color {
        ColorCapability::NoColor => "none",
        ColorCapability::Ansi16 => "ansi16",
        ColorCapability::Ansi256 => "ansi256",
        ColorCapability::TrueColor => "truecolor",
    };
    println!("color capability: {color}");
    println!("unicode borders: {}", capabilities.unicode);
    for line in terminal_key_diagnostics() {
        println!("{line}");
    }
    println!("git: {git}");
}

fn terminal_key_diagnostics() -> Vec<String> {
    vec![
        "reserved key Ctrl-C: terminal interrupt; use Ctrl-Q to quit mtide".into(),
        "reserved key Ctrl-Z: shell suspend; use the command palette for suspend-aware actions"
            .into(),
        "alternate save key: Super-S or Ctrl-S (if the terminal forwards it)".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        configured_task, configured_task_plan, default_task, default_test_task,
        diff_context_visible, editor_diff_line_marks, file_icon, find_project_config,
        horizontal_view, key_stroke, notification_presentation, parse_args, parse_force_key,
        parse_location, render_git_diff, render_git_split, sticky_context_lines, terminal_bytes,
        terminal_color_slot, terminal_key_diagnostics, write_git_diff_line, App, Branch, Edit,
        GitChangeKind, GitHistoryFilterKind, Selection, StatusEntry, TerminalSession,
    };
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use mtide_api::ColorSlot;
    use mtide_diff::{apply_hunks, diff_lines, DiffOp};
    use mtide_task::{TaskRun, TaskSpec, TaskState};
    use mtide_tui::CellBuffer;
    use mtide_vt::Color as VtColor;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn fr_gt06_git_diff_render_tracks_multiple_hunks() {
        let diff = diff_lines("a\nb\nc\n", "a\nB\nc\nD\n");
        let (lines, line_hunks, hunks) = render_git_diff(&diff);
        assert_eq!(hunks, vec![0, 1]);
        assert_eq!(line_hunks.len(), lines.len());
        assert!(lines
            .iter()
            .any(|(_, kind)| { matches!(kind, super::GitDiffLineKind::Added) }));
        assert!(diff
            .operations
            .iter()
            .any(|operation| { matches!(operation, DiffOp::Delete(line) if line == "b") }));
    }

    #[test]
    fn fr_gt06_diff_context_hides_distant_unchanged_lines() {
        let diff = diff_lines("a\nb\nc\nd\ne\nf\ng\n", "a\nB\nc\nd\ne\nF\ng\n");
        assert!(!diff_context_visible(Some(&diff), 0, 0));
        assert!(diff_context_visible(Some(&diff), 1, 0));
        assert!(!diff_context_visible(Some(&diff), 3, 0));
        assert!(diff_context_visible(Some(&diff), 3, 1));
    }

    #[test]
    fn fr_gt06_diff_syntax_keeps_change_background_and_token_foreground() {
        let mut buffer = CellBuffer::new(32, 1);
        write_git_diff_line(
            &mut buffer,
            0,
            0,
            "+",
            "fn value = 42 // note",
            super::GitDiffLineKind::Added,
            false,
            mtide_syntax::Language::Rust,
        );
        assert_eq!(
            buffer.get(1, 0).map(|cell| cell.style),
            Some(ColorSlot::Focus)
        );
        assert_eq!(
            buffer.get(1, 0).and_then(|cell| cell.background),
            Some(ColorSlot::Success)
        );
    }

    #[test]
    fn fr_gt14_editor_diff_marks_only_current_worktree_lines() {
        let diff = diff_lines("one\ntwo\nthree\n", "one\ninserted\ntwo\nthree\n");
        assert_eq!(
            editor_diff_line_marks(&diff),
            vec![
                super::GitDiffLineKind::Equal,
                super::GitDiffLineKind::Added,
                super::GitDiffLineKind::Equal,
                super::GitDiffLineKind::Equal,
            ]
        );
    }

    #[test]
    fn fr_gt14_editor_diff_rollback_preserves_other_hunks() {
        let original = "one\ntwo\nthree\nfour\n";
        let edited = "one\nTWO\nthree\nFOUR\n";
        let diff = diff_lines(original, edited);
        let remaining = apply_hunks(original, &diff, &[1]).expect("keep second hunk");
        assert_eq!(remaining, "one\ntwo\nthree\nFOUR\n");
        let restored = apply_hunks(original, &diff, &[]).expect("rollback all hunks");
        assert_eq!(restored, original);
    }

    #[test]
    fn fr_ed25_sticky_context_returns_nested_enclosing_regions_in_order() {
        let text = "fn outer() {\n    fn inner() {\n        work();\n    }\n}\n";
        assert_eq!(
            sticky_context_lines(text, mtide_syntax::Language::Rust, 2, 2),
            vec![0, 1]
        );
        assert!(sticky_context_lines(text, mtide_syntax::Language::Rust, 0, 2).is_empty());
    }

    #[test]
    fn fr_ed25_editor_draws_sticky_context_above_scrolled_content() {
        let mut app = App::open(
            None,
            false,
            Some("fn outer() {\n    fn inner() {\n        work();\n    }\n}\n".into()),
            PathBuf::from("/tmp/mtide-sticky-context-session.json"),
            None,
            None,
        )
        .expect("open sticky context document");
        app.cursor = app.document.line_to_byte(2).expect("inner body line");
        let mut frame = CellBuffer::new(80, 24);
        app.draw(&mut frame, 80, 24);
        let row = (0..80)
            .filter_map(|x| frame.get(x, 1).map(|cell| cell.symbol))
            .collect::<String>();
        assert!(row.contains("outer()"));
    }

    #[test]
    fn fr_gt16_git_repository_follows_active_file_across_workspace_roots() {
        let base =
            std::env::temp_dir().join(format!("mtide-multi-root-git-{}", std::process::id()));
        let first_root = base.join("first");
        let second_root = base.join("second");
        fs::create_dir_all(&first_root).expect("create first root");
        fs::create_dir_all(&second_root).expect("create second root");
        for root in [&first_root, &second_root] {
            let output = Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .output()
                .expect("init repository");
            assert!(output.status.success());
        }
        let first = first_root.join("first.rs");
        let second = second_root.join("second.rs");
        fs::write(&first, "fn first() {}\n").expect("write first file");
        fs::write(&second, "fn second() {}\n").expect("write second file");
        let mut app = App::open(
            Some(first.clone()),
            false,
            None,
            base.join("session.json"),
            None,
            None,
        )
        .expect("open first repository file");
        assert_eq!(
            app.git_repository.as_ref().map(|repo| repo.root()),
            Some(first_root.as_path())
        );
        app.open_file_tab(&second, false)
            .expect("open second repository file");
        assert_eq!(
            app.git_repository.as_ref().map(|repo| repo.root()),
            Some(second_root.as_path())
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn fr_gt06_git_split_render_pairs_old_and_new_lines() {
        let diff = diff_lines("same\nold\ntail\n", "same\nnew\ntail\nextra\n");
        let (_, line_hunks, _) = render_git_diff(&diff);
        let rows = render_git_split(&diff, &line_hunks);
        assert_eq!(rows[0].0, "same");
        assert_eq!(rows[0].1, "same");
        assert_eq!(rows[1].0, "old");
        assert_eq!(rows[1].1, "new");
        assert_eq!(rows[3].0, "");
        assert_eq!(rows[3].1, "extra");
    }

    #[test]
    fn fr_gt05_dirty_branch_switch_requires_an_explicit_choice() {
        let mut app = App::open(
            None,
            false,
            Some("dirty".into()),
            PathBuf::from("/tmp/mtide-branch-choice-session.json"),
            None,
            None,
        )
        .expect("open branch choice app");
        app.git_branches = vec![
            Branch {
                name: "main".into(),
                current: true,
                remote: false,
            },
            Branch {
                name: "feature".into(),
                current: false,
                remote: false,
            },
        ];
        app.git_branch_selected = 1;
        app.git_entries.push(StatusEntry {
            path: PathBuf::from("main.rs"),
            original_path: None,
            kind: GitChangeKind::Modified,
            index_code: ' ',
            worktree_code: 'M',
        });
        app.checkout_git_branch("feature", super::GitCheckoutMode::Prompt)
            .expect("request branch checkout");
        assert_eq!(app.git_checkout_pending.as_deref(), Some("feature"));
        assert!(app
            .notification
            .as_deref()
            .is_some_and(|message| message.contains("stash") && message.contains("carry")));
    }

    #[test]
    fn fr_gt09_conflict_resolution_is_undoable_and_cursor_scoped() {
        let mut app = App::open(
            None,
            false,
            Some("before\n<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\nafter\n".into()),
            PathBuf::from("/tmp/mtide-conflict-session.json"),
            None,
            None,
        )
        .expect("open source");
        app.cursor = app.document.line_to_byte(4).expect("theirs line");
        app.resolve_conflict_at_cursor(super::ConflictChoice::Theirs)
            .expect("resolve conflict");
        assert_eq!(app.document.text(), "before\ntheirs\nafter\n");
        assert_eq!(
            app.notification.as_deref(),
            Some("conflict resolved: theirs")
        );
        assert!(app.document.undo().is_some());
        assert!(app.document.text().contains("<<<<<<< ours"));
    }

    #[test]
    fn fr_gt09_all_conflict_resolution_is_one_undoable_transaction() {
        let mut app = App::open(
            None,
            false,
            Some(
                "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\nkeep\n<<<<<<< ours\nc\n=======\nd\n>>>>>>> theirs\n".into(),
            ),
            PathBuf::from("/tmp/mtide-conflict-all-session.json"),
            None,
            None,
        )
        .expect("open source");
        app.resolve_all_conflicts(super::ConflictChoice::Theirs)
            .expect("resolve all conflicts");
        assert_eq!(app.document.text(), "b\nkeep\nd\n");
        assert!(app.document.undo().is_some());
        assert_eq!(app.document.text().matches("<<<<<<<").count(), 2);
    }

    #[test]
    fn fr_gt09_conflict_restore_reinstates_original_markers() {
        let path =
            std::env::temp_dir().join(format!("mtide-conflict-restore-{}.txt", std::process::id()));
        let original = "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n";
        fs::write(&path, original).expect("write conflict file");
        let mut app = App::open(
            Some(path.clone()),
            false,
            None,
            path.with_extension("session"),
            None,
            None,
        )
        .expect("open conflict file");
        app.resolve_all_conflicts(super::ConflictChoice::Ours)
            .expect("resolve conflict");
        assert!(!app.document.text().contains("<<<<<<<"));
        app.restore_conflict_state().expect("restore conflict");
        assert_eq!(app.document.text(), original);
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr04_selected_text_is_sent_to_terminal_with_optional_enter() {
        let mut app = App::open(
            None,
            false,
            Some("echo hi\n".into()),
            PathBuf::from("/tmp/mtide-terminal-send-session.json"),
            None,
            None,
        )
        .expect("open source");
        app.selection = Some(Selection::new(0, 7));
        app.terminal_panel.sessions.add(
            TerminalSession::spawn_pty(
                "test",
                "sh",
                &[
                    "-c".into(),
                    "IFS= read -r line; printf 'got:%s\\n' \"$line\"".into(),
                ],
                None,
                80,
                6,
                100,
            )
            .expect("start terminal"),
        );
        app.send_selection_to_terminal(true)
            .expect("send selected text");
        for _ in 0..100 {
            app.terminal_panel.sessions.poll_all();
            if app
                .terminal_panel
                .sessions
                .selected()
                .is_some_and(|session| session.scrollback().any(|line| line == "got:echo hi"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let lines = app
            .terminal_panel
            .sessions
            .selected()
            .expect("selected terminal")
            .scrollback()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line == "got:echo hi"), "{lines:?}");
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr05_terminal_file_link_jumps_to_target_location() {
        let root = std::env::temp_dir().join(format!("mtide-terminal-link-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create link fixture");
        let source = root.join("main.rs");
        let target = root.join("target.rs");
        fs::write(&source, "fn main() {}\n").expect("write source");
        fs::write(&target, "zero\none\ntwo\n").expect("write target");
        let mut app = App::open(
            Some(source),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open source");
        app.terminal_panel.sessions.add(
            TerminalSession::spawn(
                "test",
                "sh",
                &["-c".into(), "printf 'target.rs:2:3\\n'".into()],
                Some(&root),
                80,
                6,
                100,
            )
            .expect("start terminal"),
        );
        for _ in 0..40 {
            app.terminal_panel.sessions.poll_all();
            if app
                .terminal_panel
                .sessions
                .selected()
                .is_some_and(|session| session.scrollback().any(|line| line == "target.rs:2:3"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        app.handle(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("jump terminal link");
        assert_eq!(app.path, Some(target));
        assert_eq!(app.document.byte_to_line(app.cursor), Some(1));
        assert_eq!(app.current_column(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr05_terminal_mouse_selection_copies_original_line_breaks() {
        let mut app = App::open(
            None,
            false,
            Some("source".into()),
            PathBuf::from("/tmp/mtide-terminal-copy-session.json"),
            None,
            None,
        )
        .expect("open source");
        let mut session = TerminalSession::spawn(
            "test",
            "sh",
            &["-c".into(), "true".into()],
            None,
            20,
            4,
            100,
        )
        .expect("start terminal");
        session.screen.feed(b"one\r\ntwo");
        app.terminal_panel.sessions.add(session);
        app.handle_terminal_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 16,
                modifiers: KeyModifiers::NONE,
            },
            24,
        );
        app.handle_terminal_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 3,
                row: 17,
                modifiers: KeyModifiers::NONE,
            },
            24,
        );
        app.handle(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("copy terminal selection");
        assert_eq!(app.clipboard, "one\ntwo");
    }

    #[test]
    fn fr_tr03_terminal_input_maps_control_unicode_and_navigation_keys() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(terminal_bytes(ctrl_c), Some(vec![3]));
        assert_eq!(key_stroke(ctrl_c).map(|stroke| stroke.key), Some('c'));

        let alt_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(terminal_bytes(alt_x), Some(vec![0x1b, b'x']));
        assert_eq!(
            terminal_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn fr_tr07_configured_force_keys_parse_modifiers_and_reject_unknown_values() {
        assert_eq!(parse_force_key("ctrl-q").map(|key| key.control), Some(true));
        assert_eq!(parse_force_key("alt-shift-x").map(|key| key.key), Some('x'));
        assert_eq!(
            parse_force_key("ctrl-backtick").map(|key| key.key),
            Some('`')
        );
        assert!(parse_force_key("ctrl-unknown").is_none());
    }

    #[test]
    fn fr_bd02_default_task_detects_project_recipe_and_run_variant() {
        let root = std::env::temp_dir().join(format!("mtide-task-recipe-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).expect("create task fixture");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .expect("write manifest");
        let file = root.join("src/main.rs");
        fs::write(&file, "fn main() {}\n").expect("write source");
        let build = default_task(Some(&file), false).expect("build recipe");
        assert_eq!(build.program, "cargo");
        assert_eq!(build.args, vec!["check"]);
        let run = default_task(Some(&file), true).expect("run recipe");
        assert_eq!(run.args, vec!["run"]);
        assert_eq!(run.cwd, Some(root.clone()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_bd02_default_task_detects_python_recipe_and_current_file_run() {
        let root = std::env::temp_dir().join(format!("mtide-python-recipe-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create python fixture");
        fs::write(root.join("pyproject.toml"), "[project]\nname='fixture'\n")
            .expect("write pyproject");
        let file = root.join("main.py");
        fs::write(&file, "print('ok')\n").expect("write python file");
        let build = default_task(Some(&file), false).expect("python build recipe");
        assert_eq!(build.program, "python");
        assert_eq!(build.args, vec!["-m", "pytest"]);
        let run = default_task(Some(&file), true).expect("python run recipe");
        assert_eq!(run.program, "python");
        assert_eq!(run.args, vec![file.to_string_lossy().into_owned()]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_bd06_default_test_task_selects_project_test_command() {
        let root = std::env::temp_dir().join(format!("mtide-test-recipe-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create test fixture");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .expect("write cargo manifest");
        let task = default_test_task(Some(&root.join("src/main.rs"))).expect("test task");
        assert_eq!(task.program, "cargo");
        assert_eq!(task.args, vec!["test"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_bd01_configured_task_loads_command_args_cwd_and_environment() {
        let root = std::env::temp_dir().join(format!("mtide-task-config-{}", std::process::id()));
        fs::create_dir_all(root.join(".mtide")).expect("create task config directory");
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write source");
        fs::write(
            root.join(".mtide/tasks.toml"),
            "[prepare]\nlabel = \"prepare\"\nprogram = \"echo\"\nargs = [\"prepare\"]\n[build]\nlabel = \"configured build\"\ncommand = \"cargo\"\nargs = [\"check\", \"--quiet\"]\ncwd = \".\"\nenv = { PROFILE = \"debug\" }\ndepends_on = [\"prepare\"]\n[run]\nprogram = \"cargo\"\nargs = [\"run\"]\n",
        )
        .expect("write task config");
        assert!(root.join(".mtide/tasks.toml").is_file());
        let parsed: toml::Table = fs::read_to_string(root.join(".mtide/tasks.toml"))
            .expect("read task config")
            .parse::<toml::Table>()
            .expect("parse task config");
        assert!(parsed.get("build").is_some());
        assert!(configured_task(&root, false).is_some());
        assert_eq!(
            configured_task_plan(&root, "build")
                .expect("configured task plan")
                .iter()
                .map(|task| task.label.as_str())
                .collect::<Vec<_>>(),
            vec!["prepare", "configured build"]
        );
        let build = default_task(Some(&file), false).expect("configured build task");
        assert_eq!(build.label, "configured build");
        assert_eq!(build.program, "cargo");
        assert_eq!(build.args, vec!["check", "--quiet"]);
        assert_eq!(build.cwd, Some(root.clone()));
        assert_eq!(build.environment.get("PROFILE"), Some(&"debug".to_owned()));
        let run = default_task(Some(&file), true).expect("configured run task");
        assert_eq!(run.args, vec!["run"]);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd01_task_dependencies_run_in_order_and_failures_stop_queue() {
        let root = std::env::temp_dir().join(format!("mtide-task-deps-{}", std::process::id()));
        fs::create_dir_all(root.join(".mtide")).expect("create dependency config directory");
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write source");
        fs::write(
            root.join(".mtide/tasks.toml"),
            "[prepare]\nprogram = \"sh\"\nargs = [\"-c\", \"printf prepare >> marker\"]\n[build]\nprogram = \"sh\"\nargs = [\"-c\", \"printf build >> marker\"]\ndepends_on = [\"prepare\"]\n",
        )
        .expect("write dependency config");
        let mut app = App::open(
            Some(file),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open dependency task app");
        app.start_task(false).expect("start dependency task");
        for _ in 0..100 {
            app.poll_task().expect("poll dependency task");
            if app.task_queue.is_empty()
                && app
                    .task_run
                    .as_ref()
                    .is_some_and(|task| !matches!(task.state(), TaskState::Running))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.task_queue.is_empty());
        assert_eq!(
            fs::read_to_string(root.join("marker")).expect("read marker"),
            "preparebuild"
        );
        fs::write(
            root.join(".mtide/tasks.toml"),
            "[prepare]\nprogram = \"sh\"\nargs = [\"-c\", \"exit 7\"]\n[build]\nprogram = \"sh\"\nargs = [\"-c\", \"printf should-not-run >> marker\"]\ndepends_on = [\"prepare\"]\n",
        )
        .expect("write failing dependency config");
        let mut failing_app = App::open(
            Some(root.join("main.rs")),
            false,
            None,
            root.join("failure-session.json"),
            None,
            None,
        )
        .expect("open failing dependency app");
        failing_app
            .start_task(false)
            .expect("start failing dependency");
        for _ in 0..100 {
            failing_app.poll_task().expect("poll failing dependency");
            if failing_app.task_queue.is_empty()
                && failing_app
                    .task_run
                    .as_ref()
                    .is_some_and(|task| !matches!(task.state(), TaskState::Running))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(failing_app.task_queue.is_empty());
        assert_eq!(
            fs::read_to_string(root.join("marker")).expect("read failure marker"),
            "preparebuild"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd05_task_output_panel_supports_search_and_copy() {
        let root = std::env::temp_dir().join(format!("mtide-task-output-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create task output fixture");
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write task output source");
        let mut app = App::open(
            Some(file),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open task output app");
        app.task_run = Some(
            TaskRun::start(TaskSpec {
                label: "output".into(),
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'src/main.rs:1:1: error: bad value\\nsecond line\\n'; exit 1".into(),
                ],
                cwd: Some(root.clone()),
                environment: std::collections::BTreeMap::new(),
            })
            .expect("start output task"),
        );
        app.task_started = Some(Instant::now());
        for _ in 0..100 {
            app.poll_task().expect("poll output task");
            if app.task_run.as_ref().is_some_and(|task| {
                !matches!(task.state(), TaskState::Running) && task.output().contains("bad value")
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            app.task_output_open,
            "failed task should open its output panel"
        );
        app.handle(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("open output panel");
        assert!(app.task_output_open);
        app.handle(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("copy task output");
        assert!(app.clipboard.contains("bad value"));
        app.handle(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .expect("start output search");
        for character in "second".chars() {
            app.handle(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type output search");
        }
        assert_eq!(app.task_output_search.as_deref(), Some("second"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd05_completed_task_outputs_are_switchable_tabs() {
        let root = std::env::temp_dir().join(format!("mtide-task-tabs-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create task tab fixture");
        let mut app = App::open(
            None,
            false,
            Some("buffer".into()),
            root.join("session.json"),
            None,
            None,
        )
        .expect("open task tab app");
        app.launch_task_spec(TaskSpec {
            label: "first".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "printf first-output".into()],
            cwd: Some(root.clone()),
            environment: std::collections::BTreeMap::new(),
        })
        .expect("launch first task");
        for _ in 0..100 {
            app.poll_task().expect("poll first task");
            if app
                .task_run
                .as_ref()
                .is_some_and(|task| !matches!(task.state(), TaskState::Running))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.task_output_history.len(), 1);
        assert!(
            !app.task_output_open,
            "successful task should collapse its output panel"
        );
        app.task_output_tab = 1;
        let (_, output, _) = app.selected_task_output().expect("select prior output");
        assert!(output.contains("first-output"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd07_different_tasks_run_in_parallel_and_keep_separate_output_tabs() {
        let mut app = App::open(
            None,
            false,
            Some("buffer".into()),
            PathBuf::from("/tmp/mtide-parallel-task-session.json"),
            None,
            None,
        )
        .expect("open task app");
        app.launch_task_spec(TaskSpec {
            label: "first".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 0.05; printf first".into()],
            cwd: None,
            environment: std::collections::BTreeMap::new(),
        })
        .expect("launch first task");
        app.start_task_named("second", || {
            Some(TaskSpec {
                label: "second".into(),
                program: "sh".into(),
                args: vec!["-c".into(), "printf second".into()],
                cwd: None,
                environment: std::collections::BTreeMap::new(),
            })
        })
        .expect("launch second task");
        assert_eq!(app.background_tasks.len(), 1);
        assert_eq!(app.task_run.as_ref().map(TaskRun::label), Some("second"));
        for _ in 0..100 {
            app.poll_task().expect("poll parallel tasks");
            if app
                .task_run
                .as_ref()
                .is_some_and(|task| !matches!(task.state(), TaskState::Running))
                && app.background_tasks.is_empty()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.background_tasks.is_empty());
        assert!(app
            .task_output_history
            .iter()
            .any(|snapshot| snapshot.label == "first"));
        let _ = fs::remove_file("/tmp/mtide-parallel-task-session.json");
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd03_task_problem_jumps_to_file_line_and_column() {
        let root = std::env::temp_dir().join(format!("mtide-task-jump-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).expect("create task fixture");
        let file = root.join("src/main.rs");
        fs::write(&file, "zero\nlet value = 1;\n").expect("write source");
        let mut app = App::open(
            Some(file.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open source");
        app.task_run = Some(
            TaskRun::start(TaskSpec {
                label: "diagnostics".into(),
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'src/main.rs:2:5: error: bad value\\n'".into(),
                ],
                cwd: Some(root.clone()),
                environment: std::collections::BTreeMap::new(),
            })
            .expect("start diagnostics task"),
        );
        for _ in 0..20 {
            app.poll_task().expect("poll diagnostics");
            if app
                .task_run
                .as_ref()
                .is_some_and(|task| !task.problems().is_empty())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        app.jump_to_first_problem().expect("jump to diagnostic");
        assert_eq!(app.path, Some(file));
        assert_eq!(app.document.byte_to_line(app.cursor), Some(1));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd03_task_results_jump_to_selected_problem() {
        let root = std::env::temp_dir().join(format!("mtide-task-selected-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create task fixture");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        fs::write(&first, "first\n").expect("write first source");
        fs::write(&second, "second\n").expect("write second source");
        let mut app = App::open(
            Some(first.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open source");
        app.task_run = Some(
            TaskRun::start(TaskSpec {
                label: "diagnostics".into(),
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'first.rs:1:1: error: first\\nsecond.rs:1:1: error: second\\n'".into(),
                ],
                cwd: Some(root.clone()),
                environment: std::collections::BTreeMap::new(),
            })
            .expect("start diagnostics task"),
        );
        for _ in 0..20 {
            app.poll_task().expect("poll diagnostics");
            if app
                .task_run
                .as_ref()
                .is_some_and(|task| task.problems().len() >= 2)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        app.jump_to_problem_index(1)
            .expect("jump to selected diagnostic");
        assert_eq!(app.path, Some(second));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_gt12_git_status_refresh_runs_off_ui_thread() {
        let root = std::env::temp_dir().join(format!("mtide-git-async-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create git fixture");
        let file = root.join("main.rs");
        fs::write(&file, "fn main() {}\n").expect("write source");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "mtide@example.test"],
            vec!["config", "user.name", "mtide-test"],
            vec!["add", "main.rs"],
            vec!["commit", "-qm", "initial"],
        ] {
            assert!(Command::new("git")
                .current_dir(&root)
                .args(args)
                .status()
                .expect("run git")
                .success());
        }
        fs::write(root.join("untracked.txt"), "pending\n").expect("write untracked");
        let mut app = App::open(
            Some(file),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open git source");
        app.last_git_poll = Instant::now() - Duration::from_secs(1);
        app.poll_git().expect("schedule git refresh");
        assert!(app.git_status_job.is_some());
        for _ in 0..100 {
            app.poll_git().expect("poll git refresh");
            if app.git_status_job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.git_status_job.is_none());
        assert!(app.git_branch.is_some());
        assert_eq!(app.git_counts.2, 1);
        assert_eq!(app.git_history.len(), 1);
        app.git_panel_open = true;
        app.handle(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("toggle git tree view");
        assert!(app.git_tree_mode);
        app.git_history_mode = true;
        app.open_git_history_detail();
        for _ in 0..100 {
            app.poll_git().expect("poll commit detail");
            if app.git_history_detail_job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.git_history_detail.is_some());
        assert!(app
            .git_history_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("main.rs")));
        app.git_panel_open = true;
        app.git_history_mode = true;
        app.git_history_filter_kind = GitHistoryFilterKind::Keyword;
        app.git_history_filter = "initial".into();
        app.git_history_file_only = true;
        app.refresh_git_history();
        for _ in 0..100 {
            app.poll_git().expect("poll filtered history");
            if app.git_status_job.is_none() && !app.git_history.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.git_history.len(), 1);
        assert_eq!(app.git_history[0].subject, "initial");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed17_timed_autosave_writes_named_buffer_and_honors_external_conflict() {
        let root = std::env::temp_dir().join(format!("mtide-autosave-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create autosave fixture");
        let file = root.join("main.rs");
        fs::write(&file, "one\n").expect("write initial file");
        let mut app = App::open(
            Some(file.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open file");
        app.document
            .apply(&[Edit {
                range: 0..3,
                insert: "two".into(),
            }])
            .expect("edit file");
        app.autosave_interval = Duration::from_millis(1);
        app.last_autosave = Instant::now() - Duration::from_secs(1);
        app.poll_autosave();
        assert_eq!(
            fs::read_to_string(&file).expect("read autosaved file"),
            "two\n"
        );
        app.document
            .apply(&[Edit {
                range: 0..3,
                insert: "bad".into(),
            }])
            .expect("edit after save");
        fs::write(&file, "external\n").expect("write external change");
        app.last_autosave = Instant::now() - Duration::from_secs(1);
        app.poll_autosave();
        assert!(app.external_change);
        assert_eq!(
            fs::read_to_string(&file).expect("read external file"),
            "external\n"
        );
    }

    #[test]
    fn fr_ed19_save_all_on_quit_persists_named_dirty_buffer() {
        let root = std::env::temp_dir().join(format!("mtide-save-all-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create save-all fixture");
        let file = root.join("main.rs");
        fs::write(&file, "before\n").expect("write initial file");
        let mut app = App::open(
            Some(file.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open file");
        app.document
            .apply(&[Edit {
                range: 0..6,
                insert: "after".into(),
            }])
            .expect("edit file");
        app.save_all_on_quit = true;
        app.shutdown().expect("save all on quit");
        assert_eq!(
            fs::read_to_string(file).expect("read saved file"),
            "after\n"
        );
    }

    #[test]
    fn fr_ed20_stdin_buffer_uses_save_as_before_writing() {
        let target = std::env::temp_dir().join(format!(
            "mtide-stdin-save-as-{}-{}.txt",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let mut app = App::open(
            None,
            false,
            Some("git diff --no-index\n".into()),
            PathBuf::from("/tmp/mtide-stdin-save-as-session.json"),
            None,
            None,
        )
        .expect("open stdin buffer");
        app.handle(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("open save-as dialog");
        assert!(app.save_as_open);
        app.save_as_input = target.to_string_lossy().into_owned();
        app.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("save stdin buffer");
        assert_eq!(
            fs::read_to_string(&target).expect("read saved buffer"),
            "git diff --no-index\n"
        );
        assert_eq!(app.path.as_deref(), Some(target.as_path()));
        let _ = fs::remove_file(target);
    }

    #[cfg(unix)]
    #[test]
    fn fr_bd07_ctrl_shift_f9_cancels_running_task_from_app() {
        let mut app = App::open(
            None,
            false,
            Some("source".into()),
            PathBuf::from("/tmp/mtide-task-cancel-session.json"),
            None,
            None,
        )
        .expect("open source");
        app.task_run = Some(
            TaskRun::start(TaskSpec {
                label: "long".into(),
                program: "sh".into(),
                args: vec!["-c".into(), "sleep 10".into()],
                cwd: None,
                environment: std::collections::BTreeMap::new(),
            })
            .expect("start task"),
        );
        app.handle(KeyEvent::new(
            KeyCode::F(9),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("cancel task");
        assert!(app
            .task_run
            .as_ref()
            .is_some_and(|task| matches!(task.state(), TaskState::Cancelled)));
    }

    #[test]
    fn fr_ed06_parses_file_line_column_without_touching_windows_like_paths() {
        assert_eq!(
            parse_location("src/main.rs:42:5"),
            ("src/main.rs", Some((42, 5)))
        );
        assert_eq!(parse_location("README.md"), ("README.md", None));
        assert_eq!(
            parse_location("C:\\work\\main.rs"),
            ("C:\\work\\main.rs", None)
        );
    }

    #[test]
    fn fr_ws04_cli_parses_wait_stdin_theme_and_location() {
        let options = parse_args([
            "--wait".into(),
            "--readonly".into(),
            "--safe-mode".into(),
            "--no-lsp".into(),
            "--no-plugins".into(),
            "--log-level".into(),
            "warn".into(),
            "--theme".into(),
            "modern".into(),
            "--workspace".into(),
            "project.mtide".into(),
            "src/main.rs:12:7".into(),
        ]);
        assert!(options.wait);
        assert!(options.readonly);
        assert!(options.safe_mode);
        assert!(options.no_lsp);
        assert!(options.no_plugins);
        assert_eq!(options.log_level.as_deref(), Some("warn"));
        assert!(!options.stdin);
        assert_eq!(options.theme_override.as_deref(), Some("modern"));
        assert_eq!(options.workspace_file, Some(PathBuf::from("project.mtide")));
        assert_eq!(options.initial_position, Some((12, 7)));
    }

    #[test]
    fn fr_ui19_terminal_diagnostics_explain_reserved_keys_and_alternatives() {
        let diagnostics = terminal_key_diagnostics().join("\n");
        assert!(diagnostics.contains("Ctrl-C"));
        assert!(diagnostics.contains("Ctrl-Q"));
        assert!(diagnostics.contains("Super-S"));
    }

    #[test]
    fn fr_ws01_cli_collects_additional_workspace_paths() {
        let options = parse_args(["one.rs".into(), "./second".into(), "third.rs".into()]);
        assert_eq!(options.path, Some(PathBuf::from("one.rs")));
        assert_eq!(
            options.additional_paths,
            vec![PathBuf::from("./second"), PathBuf::from("third.rs")]
        );
    }

    #[test]
    fn fr_ed06_ctrl_g_jumps_to_symbol_and_alt_left_returns() {
        let mut app = App::open(
            None,
            false,
            Some("fn first() {}\n\nfn second() {}\n".into()),
            PathBuf::from("/tmp/mtide-navigation-session.json"),
            None,
            None,
        )
        .expect("open navigation document");
        app.handle(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .expect("jump to symbol");
        assert_eq!(app.document.byte_to_line(app.cursor), Some(2));
        app.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))
            .expect("navigate back");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn fr_ui05_status_line_contains_core_state_and_fits_narrow_terminal() {
        let mut app = App::open(
            None,
            true,
            Some("你好".into()),
            PathBuf::from("/tmp/mtide-status-session.json"),
            None,
            Some("modern".into()),
        )
        .expect("open status document");
        app.cursor = app.document.text().len();
        app.config_warning = true;
        let status = app.status_line(160, app.document.text().len());
        assert!(status.chars().count() <= 160);
        assert!(status.contains("Ln 1"));
        assert!(status.contains("readonly"));
        app.handle(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("toggle soft wrap");
        assert!(app
            .status_line(160, app.document.text().len())
            .contains("wrap=off"));
    }

    #[test]
    fn fr_ui01_app_draws_sidebar_on_wide_terminal_and_collapses_it_on_narrow() {
        let mut app = App::open(
            None,
            false,
            Some("hello".into()),
            PathBuf::from("/tmp/mtide-layout-session.json"),
            None,
            None,
        )
        .expect("open layout document");
        let mut wide = CellBuffer::new(120, 40);
        app.draw(&mut wide, 120, 40);
        assert_eq!(wide.get(1, 1).map(|cell| cell.symbol), Some('W'));
        let mut narrow = CellBuffer::new(80, 24);
        app.draw(&mut narrow, 80, 24);
        assert_ne!(narrow.get(1, 1).map(|cell| cell.symbol), Some('W'));
        app.handle(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL))
            .expect("toggle editor split");
        let mut split = CellBuffer::new(120, 40);
        app.draw(&mut split, 120, 40);
        assert!(app.editor_split);
        assert_eq!(split.get(72, 2).map(|cell| cell.symbol), Some('│'));
        app.handle(KeyEvent::new(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("toggle horizontal editor split");
        let mut horizontal = CellBuffer::new(120, 40);
        app.draw(&mut horizontal, 120, 40);
        assert!(app.editor_split_horizontal);
        assert_eq!(horizontal.get(24, 20).map(|cell| cell.symbol), Some('─'));
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 24,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            120,
            40,
        );
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 32,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            120,
            40,
        );
        assert_eq!(app.sidebar_width, Some(32));
    }

    #[test]
    fn fr_ui12_editor_scrollbar_marks_search_lines_and_current_line() {
        let mut app = App::open(
            None,
            false,
            Some("zero\none\ntarget\nthree\nfour".into()),
            PathBuf::from("/tmp/mtide-scrollbar-session.json"),
            None,
            None,
        )
        .expect("open scrollbar document");
        app.search_query = "target".into();
        app.searching = true;
        app.refresh_search();
        app.cursor = app.document.line_column_to_byte(4, 0).expect("cursor");
        let mut frame = CellBuffer::new(80, 24);
        app.draw(&mut frame, 80, 24);
        assert_eq!(frame.get(79, 12).map(|cell| cell.symbol), Some('•'));
        assert_eq!(frame.get(79, 22).map(|cell| cell.symbol), Some('█'));
        app.scrollbar_mode = "hidden".into();
        let mut hidden = CellBuffer::new(80, 24);
        app.draw(&mut hidden, 80, 24);
        assert_eq!(hidden.get(79, 12).map(|cell| cell.symbol), Some(' '));
        assert!(app
            .status_line(160, app.document.text().len())
            .contains("scroll:hidden"));
    }

    #[test]
    fn fr_ui12_clicking_editor_scrollbar_jumps_to_document_position() {
        let mut app = App::open(
            None,
            false,
            Some("zero\none\ntwo\nthree\nfour".into()),
            PathBuf::from("/tmp/mtide-scrollbar-click-session.json"),
            None,
            None,
        )
        .expect("open scrollbar click document");
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 79,
                row: 22,
                modifiers: KeyModifiers::NONE,
            },
            80,
            24,
        );
        assert_eq!(app.document.byte_to_line(app.cursor), Some(4));
        assert!(app
            .notification
            .as_deref()
            .is_some_and(|message| message.contains("line 5")));
    }

    #[test]
    fn fr_ui15_file_icons_have_ascii_fallbacks() {
        assert_eq!(file_icon(Some(Path::new("main.rs"))), "R");
        assert_eq!(file_icon(Some(Path::new("README.md"))), "M");
        assert_eq!(file_icon(Some(Path::new("unknown.bin"))), "F");
        assert_eq!(file_icon(None), "?");
    }

    #[test]
    fn fr_ui08_notification_history_is_keyboard_accessible() {
        let mut app = App::open(
            None,
            false,
            Some("hello".into()),
            PathBuf::from("/tmp/mtide-notification-session.json"),
            None,
            None,
        )
        .expect("open notification document");
        app.notification = Some("build completed".into());
        let mut frame = CellBuffer::new(80, 24);
        app.draw(&mut frame, 80, 24);
        assert_eq!(app.notification_history, vec!["build completed"]);
        app.handle(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE))
            .expect("open notification history");
        assert!(app.notifications_open);
        app.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close notification history");
        assert!(!app.notifications_open);
    }

    #[test]
    fn fr_ui08_notifications_have_semantic_severity_markers() {
        assert_eq!(
            notification_presentation("commit failed: hook error"),
            ("✖", ColorSlot::Error)
        );
        assert_eq!(
            notification_presentation("task started"),
            ("…", ColorSlot::Focus)
        );
        assert_eq!(
            notification_presentation("workspace saved"),
            ("✔", ColorSlot::Success)
        );
    }

    #[test]
    fn fr_tr01_terminal_ansi_colors_reach_tui_cells() {
        assert_eq!(
            terminal_color_slot(VtColor::Indexed(1)),
            ColorSlot::Indexed(1)
        );
        assert_eq!(
            terminal_color_slot(VtColor::Indexed(2)),
            ColorSlot::Indexed(2)
        );
        assert_eq!(
            terminal_color_slot(VtColor::Rgb(20, 30, 40)),
            ColorSlot::Rgb(20, 30, 40)
        );
        assert_eq!(terminal_color_slot(VtColor::Default), ColorSlot::Default);
    }

    #[test]
    fn fr_ui07_editor_mouse_click_and_drag_select_graphemes() {
        let mut app = App::open(
            None,
            false,
            Some("hello\nworld".into()),
            PathBuf::from("/tmp/mtide-editor-mouse-session.json"),
            None,
            None,
        )
        .expect("open editor mouse document");
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            80,
            24,
        );
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 10,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            80,
            24,
        );
        assert_eq!(app.selection.map(Selection::range), Some(6..11));
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 10,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            80,
            24,
        );
        assert!(app.mouse_selection_anchor.is_none());
    }

    #[test]
    fn fr_ui07_clicking_tab_bar_activates_the_selected_document() {
        let root = std::env::temp_dir().join(format!("mtide-tab-click-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create tab click fixture");
        let first = root.join("first.rs");
        let second = root.join("第二.rs");
        fs::write(&first, "first\n").expect("write first tab");
        fs::write(&second, "second\n").expect("write second tab");
        let mut app = App::open(
            Some(first.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open first tab");
        app.open_file_tab(&second, false).expect("open second tab");
        assert_eq!(app.active_tab, 1);
        app.switch_tab(-1);
        assert_eq!(app.active_tab, 0);
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            80,
            24,
        );
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.path.as_deref(), Some(second.as_path()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ui07_shift_mouse_wheel_scrolls_editor_horizontally() {
        let mut app = App::open(
            None,
            false,
            Some("0123456789abcdefghijklmnopqrstuvwxyz".into()),
            PathBuf::from("/tmp/mtide-horizontal-mouse-session.json"),
            None,
            None,
        )
        .expect("open horizontal mouse document");
        app.horizontal_scroll = 0;
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 20,
                row: 5,
                modifiers: KeyModifiers::SHIFT,
            },
            80,
            24,
        );
        assert_eq!(app.horizontal_scroll, 3);
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 20,
                row: 5,
                modifiers: KeyModifiers::SHIFT,
            },
            80,
            24,
        );
        assert_eq!(app.horizontal_scroll, 0);
    }

    #[test]
    fn fr_nv04_bookmarks_toggle_render_and_persist() {
        let session = std::env::temp_dir().join(format!(
            "mtide-bookmarks-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let mut app = App::open(
            None,
            false,
            Some("one\ntwo\nthree".into()),
            session.clone(),
            None,
            None,
        )
        .expect("open bookmark document");
        app.cursor = app.document.line_column_to_byte(1, 0).expect("line");
        app.handle(KeyEvent::new(KeyCode::F(11), KeyModifiers::NONE))
            .expect("toggle bookmark");
        assert!(app.bookmarks.contains(&1));
        let mut frame = CellBuffer::new(80, 24);
        app.draw(&mut frame, 80, 24);
        assert_eq!(frame.get(4, 2).map(|cell| cell.symbol), Some('◆'));
        app.persist_session().expect("persist bookmarks");
        let snapshot = mtide_workspace::load_session(&session).expect("load bookmarks");
        assert_eq!(snapshot.bookmarks, vec![1]);
        let _ = fs::remove_file(session);
    }

    #[test]
    fn fr_nv01_recent_files_are_bounded_and_keyboard_selectable() {
        let session = PathBuf::from("/tmp/mtide-recent-files-session.json");
        let mut app = App::open(None, false, Some("buffer".into()), session, None, None)
            .expect("open recent file document");
        app.record_recent_file(Path::new("/tmp/first.rs"));
        app.record_recent_file(Path::new("/tmp/second.rs"));
        app.handle(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .expect("open recent files");
        assert!(app.recent_open);
        for character in "second".chars() {
            app.handle(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("filter recent files");
        }
        assert_eq!(
            app.filtered_recent_files(),
            vec![PathBuf::from("/tmp/second.rs")]
        );
        app.handle(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("edit recent filter");
        app.recent_query.clear();
        app.handle(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .expect("select recent file");
        assert_eq!(app.recent_selected, 1);
        app.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close recent files");
        assert!(!app.recent_open);
    }

    #[cfg(unix)]
    #[test]
    fn fr_sr01_ctrl_p_file_finder_searches_workspace_and_opens_match() {
        let root = std::env::temp_dir().join(format!("mtide-file-finder-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).expect("create finder fixture");
        let current = root.join("main.rs");
        let target = root.join("src/main_runner.rs");
        fs::write(&current, "current\n").expect("write current");
        fs::write(&target, "target\n").expect("write target");
        let mut app = App::open(
            Some(current),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open finder app");
        app.open_file_finder();
        for character in "mnrs".chars() {
            app.file_finder_query.push(character);
        }
        app.start_file_finder_search();
        for _ in 0..100 {
            app.poll_file_finder();
            if app.file_finder_job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.file_finder_results.contains(&target));
        app.file_finder_selected = app
            .file_finder_results
            .iter()
            .position(|path| path == &target)
            .expect("target result");
        app.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open finder result");
        assert_eq!(app.path, Some(target));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_nv09_smart_selection_expands_and_shrinks_by_editor_structure() {
        let mut app = App::open(
            None,
            false,
            Some("alpha + beta\nnext".into()),
            PathBuf::from("/tmp/mtide-smart-selection-session.json"),
            None,
            None,
        )
        .expect("open smart selection document");
        app.cursor = app.document.line_column_to_byte(0, 2).expect("cursor");
        assert_eq!(app.cursor, 2);
        assert_eq!(app.smart_selection_range(), 0..5);
        app.handle(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("expand word");
        let first = app.selection.expect("word selection").range();
        assert_eq!(&app.document.text()[first.clone()], "alpha");
        app.handle(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("expand line");
        let second = app.selection.expect("line selection").range();
        assert_eq!(&app.document.text()[second], "alpha + beta");
        app.handle(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("shrink selection");
        assert_eq!(app.selection.expect("restored word").range(), first);
    }

    #[test]
    fn fr_ui14_zen_mode_hides_side_and_bottom_panels() {
        let mut app = App::open(
            None,
            false,
            Some("hello".into()),
            PathBuf::from("/tmp/mtide-zen-session.json"),
            None,
            None,
        )
        .expect("open zen document");
        let mut normal = CellBuffer::new(120, 40);
        app.draw(&mut normal, 120, 40);
        assert_eq!(normal.get(1, 1).map(|cell| cell.symbol), Some('W'));
        app.handle(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("enable zen mode");
        let mut zen = CellBuffer::new(120, 40);
        app.draw(&mut zen, 120, 40);
        assert_ne!(zen.get(1, 1).map(|cell| cell.symbol), Some('W'));
        assert!(app.zen_mode);
        app.handle(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("disable zen mode");
        assert!(!app.zen_mode);
    }

    #[test]
    fn fr_ui02_theme_toggle_persists_with_workspace() {
        let root = std::env::temp_dir().join(format!("mtide-theme-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create theme workspace");
        let session = root.join("session.json");
        let mut app = App::open(None, false, None, session, None, None).expect("open app");
        app.workspace_file = Some(root.join("project.mtide"));
        assert_eq!(app.theme, "retro");
        app.toggle_theme().expect("toggle theme");
        assert_eq!(app.theme, "modern");
        let loaded = mtide_workspace::WorkspaceFile::load(&root.join("project.mtide"))
            .expect("load workspace");
        assert_eq!(loaded.layout.get("theme"), Some(&"modern".to_owned()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ui02_theme_preview_can_be_cancelled_or_applied() {
        let root = std::env::temp_dir().join(format!("mtide-theme-preview-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create preview workspace");
        let mut app = App::open(None, false, None, root.join("session.json"), None, None)
            .expect("open preview app");
        app.workspace_file = Some(root.join("project.mtide"));
        app.begin_theme_preview();
        assert_eq!(app.theme, "retro");
        assert_eq!(app.theme_preview.as_deref(), Some("modern"));
        app.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel preview");
        assert!(app.theme_preview.is_none());
        assert_eq!(app.theme, "retro");
        app.begin_theme_preview();
        app.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("apply preview");
        assert_eq!(app.theme, "modern");
        assert!(app.theme_preview.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ui04_keymap_preset_switch_is_persisted_and_routes_emacs_quit() {
        let root = std::env::temp_dir().join(format!("mtide-keymap-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create keymap workspace");
        let mut app = App::open(None, false, None, root.join("session.json"), None, None)
            .expect("open keymap app");
        app.workspace_file = Some(root.join("project.mtide"));
        app.set_keymap("emacs").expect("switch keymap");
        assert_eq!(app.keymap_preset, "emacs");
        assert!(app
            .handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .expect("route emacs quit"));
        let workspace = mtide_workspace::WorkspaceFile::load(&root.join("project.mtide"))
            .expect("load workspace");
        assert_eq!(workspace.layout.get("keymap"), Some(&"emacs".to_owned()));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fr_tr02_terminal_sessions_are_named_selectable_and_closable() {
        let mut app = App::open(
            None,
            false,
            None,
            PathBuf::from("/tmp/mtide-terminal-sessions.json"),
            None,
            None,
        )
        .expect("open app");
        app.new_terminal_session().expect("create first terminal");
        app.new_terminal_session().expect("create second terminal");
        assert_eq!(app.terminal_panel.sessions.len(), 2);
        assert_eq!(
            app.terminal_panel
                .sessions
                .selected()
                .map(|session| session.name()),
            Some("shell-2")
        );
        app.open_terminal_rename();
        app.terminal_rename_input = "build-shell".into();
        app.apply_terminal_rename();
        assert_eq!(
            app.terminal_panel
                .sessions
                .selected()
                .map(|session| session.name()),
            Some("build-shell")
        );
        app.terminal_panel.sessions.previous();
        assert_eq!(
            app.terminal_panel
                .sessions
                .selected()
                .map(|session| session.name()),
            Some("shell")
        );
        app.close_terminal_session();
        app.close_terminal_session();
        assert!(app.terminal_panel.sessions.is_empty());
    }

    #[test]
    fn fr_ui03_command_palette_filters_and_executes_registered_command() {
        let mut app = App::open(
            None,
            false,
            Some("hello".into()),
            PathBuf::from("/tmp/mtide-command-palette-session.json"),
            None,
            None,
        )
        .expect("open command palette document");
        app.handle(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("open command palette");
        assert!(app.command_palette);
        for character in "zen".chars() {
            app.handle(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type command search");
        }
        app.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("execute command");
        assert!(app.zen_mode);
        assert!(!app.command_palette);
    }

    #[test]
    fn fr_ui13_f1_key_help_lists_registered_shortcuts_and_closes() {
        let mut app = App::open(
            None,
            false,
            Some("hello".into()),
            PathBuf::from("/tmp/mtide-key-help-session.json"),
            None,
            None,
        )
        .expect("open key help document");
        app.handle(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))
            .expect("open key help");
        assert!(app.key_help);
        for character in "save".chars() {
            app.handle(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("filter key help");
        }
        assert_eq!(app.key_help_query, "save");
        let mut frame = CellBuffer::new(80, 24);
        app.draw(&mut frame, 80, 24);
        let has_title = (0..24).any(|y| {
            (0..80)
                .filter_map(|x| frame.get(x, y).map(|cell| cell.symbol))
                .collect::<String>()
                .contains("Keyboard shortcuts")
        });
        assert!(has_title);
        app.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close key help");
        assert!(!app.key_help);
    }

    #[test]
    fn fr_ed15_tabs_preserve_dirty_documents_and_replace_only_unpinned_preview() {
        let root = std::env::temp_dir().join(format!("mtide-tabs-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create tab fixture");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        let third = root.join("third.rs");
        fs::write(&first, "first\n").expect("write first");
        fs::write(&second, "second\n").expect("write second");
        fs::write(&third, "third\n").expect("write third");
        let mut app = App::open(
            Some(first.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open first");
        app.open_file_tab(&second, true).expect("open preview tab");
        assert_eq!(app.tabs.len(), 2);
        app.document
            .apply(&[Edit {
                range: 0..6,
                insert: "changed".into(),
            }])
            .expect("edit preview");
        app.switch_tab(-1);
        assert_eq!(app.path, Some(first.clone()));
        app.switch_tab(1);
        assert_eq!(app.path, Some(second.clone()));
        assert_eq!(app.document.text(), "changed\n");
        app.pin_current_tab();
        app.open_file_tab(&third, true)
            .expect("open second preview");
        assert_eq!(app.tabs.len(), 3);
        app.open_file_tab(&first, true)
            .expect("select existing tab");
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(app.path, Some(first.clone()));
        app.persist_session().expect("persist tabs");
        let mut restored = App::open(None, false, None, root.join("session.json"), None, None)
            .expect("restore tabs");
        assert_eq!(restored.tabs.len(), 3);
        restored.switch_tab(1);
        assert_eq!(restored.path, Some(second));
        assert_eq!(restored.document.text(), "changed\n");
        assert!(restored.document.is_dirty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed07_toggle_fold_hides_region_and_renders_summary() {
        let mut app = App::open(
            None,
            false,
            Some("fn main() {\n  let value = 1;\n}\n".into()),
            PathBuf::from("/tmp/mtide-fold-session.json"),
            None,
            None,
        )
        .expect("open fold document");
        app.toggle_fold_at_cursor();
        let mut frame = CellBuffer::new(80, 24);
        app.draw(&mut frame, 80, 24);
        assert!(frame.get(5, 1).is_some_and(|cell| cell.symbol == '…'));
        app.set_all_folds(false);
        assert!(app.folded_lines.is_empty());
        app.set_all_folds(true);
        assert_eq!(app.folded_lines, [0].into_iter().collect());
    }

    #[test]
    fn fr_ed12_large_file_mode_activates_by_line_count_and_is_visible() {
        let text = "x\n".repeat(500_000);
        let app = App::open(
            None,
            false,
            Some(text),
            PathBuf::from("/tmp/mtide-large-session.json"),
            None,
            None,
        )
        .expect("open large document");
        assert!(app.large_file_mode);
        assert!(app
            .status_line(160, app.document.text().len())
            .contains("large-file"));
    }

    #[cfg(unix)]
    #[test]
    fn fr_ed14_readonly_file_is_opened_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("mtide-readonly-{}.txt", std::process::id()));
        fs::write(&path, "locked").expect("write readonly fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("set readonly");
        let app = App::open(
            Some(path.clone()),
            false,
            None,
            PathBuf::from("/tmp/mtide-readonly-session.json"),
            None,
            None,
        )
        .expect("open readonly file");
        assert!(app.readonly);
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fr_ed11_external_save_conflict_stays_open_and_is_visible() {
        let path = std::env::temp_dir().join(format!("mtide-conflict-{}.txt", std::process::id()));
        fs::write(&path, "original").expect("write original");
        let mut app = App::open(
            Some(path.clone()),
            false,
            None,
            std::env::temp_dir().join("mtide-conflict-session.json"),
            None,
            None,
        )
        .expect("open conflict document");
        fs::write(&path, "changed outside").expect("external edit");
        app.document
            .apply(&[Edit {
                range: 0..0,
                insert: "x".into(),
            }])
            .expect("local edit");
        assert!(!app
            .handle(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("save key"));
        assert!(app.external_change);
        assert!(app
            .status_line(200, app.document.text().len())
            .contains("save blocked"));
        app.handle(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("compare external file");
        assert!(app
            .notification
            .as_deref()
            .is_some_and(|message| message.starts_with("compare:")));
        app.handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .expect("reload external file");
        assert_eq!(app.document.text(), "changed outside");
        app.document
            .apply(&[Edit {
                range: 0..0,
                insert: "local ".into(),
            }])
            .expect("local edit after reload");
        fs::write(&path, "changed again").expect("second external edit");
        app.external_change = true;
        app.handle(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("overwrite external file");
        assert_eq!(
            fs::read_to_string(&path).expect("read overwritten file"),
            "local changed outside"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fr_ws05_clean_document_reloads_external_change() {
        let root = std::env::temp_dir().join(format!("mtide-reload-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create reload fixture");
        let path = root.join("document.txt");
        fs::write(&path, "before").expect("write original");
        let mut app = App::open(
            Some(path.clone()),
            false,
            None,
            std::env::temp_dir().join("mtide-reload-session.json"),
            None,
            None,
        )
        .expect("open watched document");
        fs::write(&path, "after externally").expect("external edit");
        app.last_watch_poll = std::time::Instant::now() - std::time::Duration::from_millis(101);
        app.poll_workspace_changes();
        assert_eq!(app.document.text(), "after externally");
        assert!(!app.document.is_dirty());
        assert!(!app.external_change);
        assert_eq!(
            app.notification.as_deref(),
            Some("reloaded external change")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed10_open_brackets_and_quotes_auto_close_around_unicode_cursor() {
        let mut app = App::open(
            None,
            false,
            Some("中".into()),
            PathBuf::from("/tmp/mtide-bracket-session.json"),
            None,
            None,
        )
        .expect("open bracket document");
        app.cursor = app.document.text().len();
        app.handle(KeyEvent::new(KeyCode::Char('('), KeyModifiers::NONE))
            .expect("open paren");
        assert_eq!(app.document.text(), "中()");
        assert_eq!(app.cursor, "中(".len());
        app.handle(KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE))
            .expect("open quote");
        assert_eq!(app.document.text(), "中(\"\")");
        assert_eq!(app.cursor, "中(\"".len());
    }

    #[test]
    fn fr_ed21_horizontal_view_scrolls_by_grapheme_not_bytes() {
        assert_eq!(horizontal_view("一e\u{301}👩‍💻二", 1, 2), "e\u{301}👩‍💻");
        assert_eq!(horizontal_view("一e\u{301}👩‍💻二", 3, 2), "二");
    }

    #[test]
    fn fr_ed01_horizontal_editing_uses_grapheme_boundaries() {
        let mut app = App::open(
            None,
            false,
            Some("e\u{301}👩‍💻".into()),
            PathBuf::from("/tmp/mtide-missing-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.cursor = app.document.text().len();
        let emoji_start = "e\u{301}".len();
        assert_eq!(app.prev_boundary(), emoji_start);
        app.cursor = 0;
        assert_eq!(app.next_boundary(), emoji_start);
    }

    #[test]
    fn fr_ed02_shift_selection_replaces_one_grapheme_atomically() {
        let mut app = App::open(
            None,
            false,
            Some("a中🙂".into()),
            PathBuf::from("/tmp/mtide-selection-session.json"),
            None,
            None,
        )
        .expect("open selection document");
        app.cursor = app.document.text().len();
        app.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT))
            .expect("select emoji");
        assert_eq!(
            app.selection.map(mtide_core::Selection::range),
            Some("a中".len().."a中🙂".len())
        );
        app.handle(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("replace selection");
        assert_eq!(app.document.text(), "a中!");
        assert!(app.document.is_dirty());
        app.document.undo().expect("undo selection replacement");
        assert_eq!(app.document.text(), "a中🙂");
    }

    #[test]
    fn fr_ed02_alt_d_adds_a_cursor_and_inserts_atomically() {
        let mut app = App::open(
            None,
            false,
            Some("a\nb".into()),
            PathBuf::from("/tmp/mtide-multicursor-session.json"),
            None,
            None,
        )
        .expect("open multicursor document");
        app.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT))
            .expect("add cursor");
        assert_eq!(app.extra_cursors.len(), 1);
        app.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("insert at cursors");
        assert_eq!(app.document.text(), "xa\nxb");
        assert!(app.document.undo().is_some());
        assert_eq!(app.document.text(), "a\nb");
    }

    #[test]
    fn fr_ed04_clipboard_shortcuts_copy_cut_and_paste_selection() {
        let mut app = App::open(
            None,
            false,
            Some("hello world".into()),
            PathBuf::from("/tmp/mtide-clipboard-session.json"),
            None,
            None,
        )
        .expect("open clipboard document");
        app.cursor = 5;
        app.selection = Some(mtide_core::Selection::new(0, 5));
        app.handle(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("copy selection");
        assert_eq!(app.clipboard, "hello");
        app.handle(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("cut selection");
        assert_eq!(app.document.text(), " world");
        app.cursor = app.document.text().len();
        app.handle(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("paste clipboard");
        assert_eq!(app.document.text(), " worldhello");
        assert_eq!(app.clipboard_history.entries(), &["hello"]);
    }

    #[test]
    fn fr_ed08_line_shortcuts_are_atomic_and_undoable() {
        let session = std::env::temp_dir().join(format!(
            "mtide-line-actions-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let mut app = App::open(
            None,
            false,
            Some("one\ntwo\n".into()),
            session.clone(),
            None,
            None,
        )
        .expect("open line action document");
        app.handle(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("duplicate line");
        assert_eq!(app.document.text(), "one\none\ntwo\n");
        app.document.undo().expect("undo duplicate line");
        app.handle(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("delete line");
        assert_eq!(app.document.text(), "two\n");
        let _ = fs::remove_file(session);
    }

    #[test]
    fn fr_ed08_alt_vertical_moves_line_and_preserves_column() {
        let mut app = App::open(
            None,
            false,
            Some("one\ntwo\nthree\n".into()),
            PathBuf::from("/tmp/mtide-move-line-session.json"),
            None,
            None,
        )
        .expect("open move document");
        app.cursor = app.document.line_column_to_byte(1, 1).expect("cursor");
        app.handle(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT))
            .expect("move line up");
        assert_eq!(app.document.text(), "two\none\nthree\n");
        assert_eq!(app.current_column(), 1);
        app.document.undo().expect("undo line move");
        assert_eq!(app.document.text(), "one\ntwo\nthree\n");
    }

    #[test]
    fn fr_ed08_ctrl_slash_toggles_language_comment_and_undoes() {
        let path = std::env::temp_dir().join(format!("mtide-comment-{}.py", std::process::id()));
        fs::write(&path, "value = 1\n").expect("write comment fixture");
        let mut app = App::open(
            Some(path.clone()),
            false,
            None,
            PathBuf::from("/tmp/mtide-comment-session.json"),
            None,
            None,
        )
        .expect("open comment document");
        app.handle(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL))
            .expect("comment line");
        assert_eq!(app.document.text(), "# value = 1\n");
        app.document.undo().expect("undo comment");
        assert_eq!(app.document.text(), "value = 1\n");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fr_ed05_replace_current_search_match_uses_clipboard_and_undoes() {
        let mut app = App::open(
            None,
            false,
            Some("foo foo".into()),
            PathBuf::from("/tmp/mtide-replace-session.json"),
            None,
            None,
        )
        .expect("open replace document");
        app.searching = true;
        app.search_query = "foo".into();
        app.clipboard = "bar".into();
        app.refresh_search();
        app.handle(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("replace match");
        assert_eq!(app.document.text(), "bar foo");
        app.document.undo().expect("undo replacement");
        assert_eq!(app.document.text(), "foo foo");
    }

    #[test]
    fn fr_ed05_replace_all_search_matches_is_one_undoable_action() {
        let mut app = App::open(
            None,
            false,
            Some("foo foo bar".into()),
            PathBuf::from("/tmp/mtide-replace-all-session.json"),
            None,
            None,
        )
        .expect("open replace-all document");
        app.searching = true;
        app.search_query = "foo".into();
        app.clipboard = "baz".into();
        app.refresh_search();
        app.handle(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("replace all matches");
        assert_eq!(app.document.text(), "baz baz bar");
        app.document.undo().expect("undo replace all");
        assert_eq!(app.document.text(), "foo foo bar");
    }

    #[test]
    fn fr_ed05_search_can_switch_to_regex_mode() {
        let mut app = App::open(
            None,
            false,
            Some("item1 item22 other".into()),
            PathBuf::from("/tmp/mtide-regex-search-session.json"),
            None,
            None,
        )
        .expect("open regex search document");
        app.searching = true;
        app.search_query = r"item\d+".into();
        app.handle(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("toggle regex search");
        assert!(app.search_regex);
        assert_eq!(app.search_matches.len(), 2);
    }

    #[test]
    fn fr_ed05_regex_replace_all_expands_capture_groups() {
        let mut app = App::open(
            None,
            false,
            Some("name=alice name=bob".into()),
            PathBuf::from("/tmp/mtide-regex-replace-session.json"),
            None,
            None,
        )
        .expect("open regex replace document");
        app.searching = true;
        app.search_regex = true;
        app.search_query = r"name=(\w+)".into();
        app.clipboard = "user:$1".into();
        app.refresh_search();
        app.handle(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("replace regex matches");
        assert_eq!(app.document.text(), "user:alice user:bob");
    }

    #[test]
    fn fr_ws02_finds_nearest_project_config_for_open_file() {
        let root = std::env::temp_dir().join(format!("mtide-project-{}", std::process::id()));
        let nested = root.join("src").join("nested");
        fs::create_dir_all(root.join(".mtide")).expect("create project config directory");
        fs::create_dir_all(&nested).expect("create nested directory");
        let config = root.join(".mtide").join("config.toml");
        fs::write(&config, "theme = 'retro'").expect("write project config");
        assert_eq!(find_project_config(&nested.join("main.rs")), Some(config));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed01_search_backspace_removes_one_grapheme() {
        let mut app = App::open(
            None,
            false,
            Some("target".into()),
            PathBuf::from("/tmp/mtide-missing-search-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.searching = true;
        app.search_query = "e\u{301}👩‍💻".into();
        app.handle(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("handle backspace");
        assert_eq!(app.search_query, "e\u{301}");
    }

    #[test]
    fn fr_ed21_vertical_motion_tracks_wrapped_visual_rows() {
        let mut app = App::open(
            None,
            false,
            Some("abcdef\nghi".into()),
            PathBuf::from("/tmp/mtide-missing-wrap-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.word_wrap = true;
        app.wrap_width = 3;
        app.cursor = app.document.line_column_to_byte(0, 4).expect("cursor");
        assert!(app.move_visual_vertical(1));
        assert_eq!(app.document.byte_to_line(app.cursor), Some(1));
        assert_eq!(app.current_column(), 1);
    }

    #[test]
    fn fr_ed09_enter_inherits_indent_and_ed10_skips_existing_closer() {
        let mut app = App::open(
            None,
            false,
            Some("  fn main() {".into()),
            PathBuf::from("/tmp/mtide-indent-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.cursor = app.document.text().len();
        app.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("insert indented line");
        assert_eq!(app.document.text(), "  fn main() {\n      ");
        app.handle(KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE))
            .expect("insert closer");
        app.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .expect("move before closer");
        app.handle(KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE))
            .expect("skip closer");
        assert_eq!(app.document.text(), "  fn main() {\n      }");
    }

    #[test]
    fn fr_ed16_ctrl_word_navigation_is_utf8_safe() {
        let mut app = App::open(
            None,
            false,
            Some("中文 word🙂 next".into()),
            PathBuf::from("/tmp/mtide-word-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.handle(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
            .expect("move to next word");
        assert_eq!(&app.document.text()[..app.cursor], "中文");
        app.handle(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
            .expect("move over word");
        assert_eq!(&app.document.text()[..app.cursor], "中文 word🙂");
        app.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL))
            .expect("move to previous word");
        assert_eq!(&app.document.text()[..app.cursor], "中文 ");
    }

    #[test]
    fn fr_ed16_ctrl_paragraph_navigation_keeps_desired_column() {
        let mut app = App::open(
            None,
            false,
            Some("first line\nsecond\n\nthird paragraph\nfourth\n\nfifth".into()),
            PathBuf::from("/tmp/mtide-paragraph-session.json"),
            None,
            None,
        )
        .expect("open paragraph document");
        app.cursor = app
            .document
            .line_column_to_byte(3, 8)
            .expect("place cursor in paragraph");
        app.desired_column = 8;
        app.move_by_paragraph(1);
        assert_eq!(app.document.byte_to_line(app.cursor), Some(6));
        app.move_by_paragraph(-1);
        assert_eq!(app.document.byte_to_line(app.cursor), Some(3));
        assert_eq!(app.current_column(), 8);
    }

    #[test]
    fn fr_ed18_search_matches_create_bounded_multi_cursor_set() {
        let mut app = App::open(
            None,
            false,
            Some("foo foo foo".into()),
            PathBuf::from("/tmp/mtide-multicursor-session.json"),
            None,
            None,
        )
        .expect("open multi-cursor document");
        app.search_query = "foo".into();
        app.refresh_search();
        app.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .expect("add next search cursor");
        assert_eq!(app.extra_cursors.len(), 1);
        app.set_keymap("vscode").expect("set vscode keymap");
        app.handle(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start vscode chord");
        app.handle(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .expect("select all search matches");
        assert_eq!(app.extra_cursors.len(), 2);
        app.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("insert at all cursors");
        assert_eq!(app.document.text(), "xfoo xfoo xfoo");
    }

    #[test]
    fn fr_ed18_multi_cursor_selection_is_capped_and_reported() {
        let text = (0..1505).map(|_| "x").collect::<Vec<_>>().join(" ");
        let mut app = App::open(
            None,
            false,
            Some(text),
            PathBuf::from("/tmp/mtide-multicursor-cap-session.json"),
            None,
            None,
        )
        .expect("open capped multi-cursor document");
        app.search_query = "x".into();
        app.refresh_search();
        app.add_cursors_for_search_matches();
        assert_eq!(app.extra_cursors.len(), super::MAX_EXTRA_CURSORS);
        assert!(app
            .notification
            .as_deref()
            .is_some_and(|message| message.contains("first")));
    }

    #[test]
    fn fr_ed22_tab_expands_builtin_snippet_and_navigates_stops() {
        let mut app = App::open(
            None,
            false,
            Some("for".into()),
            PathBuf::from("/tmp/mtide-snippet-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.cursor = 3;
        app.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("expand snippet");
        assert!(app.document.text().starts_with("for item in items"));
        assert!(app.selection.is_some());
        app.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("advance snippet stop");
        assert!(app.selection.is_some());
        app.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("advance to final snippet stop");
        app.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("finish snippet");
        assert!(app.snippet_stops.is_empty());
    }

    #[test]
    fn fr_ed23_ctrl_alt_c_converts_current_identifier_as_one_edit() {
        let mut app = App::open(
            None,
            false,
            Some("user_name".into()),
            PathBuf::from("/tmp/mtide-convert-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.cursor = app.document.text().len();
        app.handle(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("convert identifier");
        assert_eq!(app.document.text(), "userName");
        app.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo conversion");
        assert_eq!(app.document.text(), "user_name");
    }

    #[test]
    fn fr_ed24_ctrl_alt_parenthesis_surrounds_and_undoes_atomically() {
        let mut app = App::open(
            None,
            false,
            Some("expr".into()),
            PathBuf::from("/tmp/mtide-surround-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.selection = Some(mtide_core::Selection::new(0, 4));
        app.handle(KeyEvent::new(
            KeyCode::Char('('),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("surround selection");
        assert_eq!(app.document.text(), "(expr)");
        app.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo surround");
        assert_eq!(app.document.text(), "expr");
    }

    #[test]
    fn fr_ui19_editor_accepts_super_find_binding() {
        let mut app = App::open(
            None,
            false,
            Some("target".into()),
            PathBuf::from("/tmp/mtide-missing-super-session.json"),
            None,
            None,
        )
        .expect("open test document");
        app.handle(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::SUPER))
            .expect("handle super find");
        assert!(app.searching);
    }

    #[test]
    fn fr_ui19_vscode_chords_route_and_cancel_without_terminal_focus() {
        let mut app = App::open(
            None,
            false,
            Some("target".into()),
            PathBuf::from("/tmp/mtide-vscode-chord-session.json"),
            None,
            None,
        )
        .expect("open chord document");
        app.handle(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start VSCode chord");
        assert!(app.vscode_chord_started.is_some());
        app.handle(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("complete keyboard-shortcuts chord");
        assert!(app.key_help);
        app.key_help = false;
        app.handle(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start zen chord");
        app.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
            .expect("complete zen chord");
        assert!(app.zen_mode);
        app.handle(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start cancellable chord");
        app.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel chord");
        assert!(app.vscode_chord_started.is_none());
    }

    #[test]
    fn fr_ws03_corrupt_session_falls_back_to_clean_document() {
        let session =
            std::env::temp_dir().join(format!("mtide-corrupt-session-{}.json", std::process::id()));
        fs::write(&session, "{not valid json").expect("write corrupt session");
        let app = App::open(None, false, None, session.clone(), None, None)
            .expect("corrupt session must not block startup");
        assert_eq!(app.document.text(), "");
        let _ = fs::remove_file(session);
    }

    #[test]
    fn fr_ws07_command_exports_redacted_diagnostics_without_source_path() {
        let root =
            std::env::temp_dir().join(format!("mtide-diagnostics-app-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create diagnostics fixture");
        let file = root.join("secret.rs");
        fs::write(&file, "secret source\n").expect("write source");
        let session = root.join("session.json");
        let mut app = App::open(Some(file), false, None, session, None, None).expect("open app");
        app.execute_command("workspace.exportDiagnostics")
            .expect("export diagnostics");
        let target = root.join("mtide-diagnostics.json.gz");
        assert!(target.exists());
        assert!(!fs::read(target)
            .expect("read diagnostics")
            .windows(root.to_string_lossy().len())
            .any(|window| window == root.to_string_lossy().as_bytes()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws01_workspace_roots_can_be_added_and_removed_from_input() {
        let root = std::env::temp_dir().join(format!("mtide-roots-{}", std::process::id()));
        let extra = root.join("extra");
        fs::create_dir_all(&extra).expect("create roots");
        let session = root.join("session.json");
        let mut app = App::open(None, false, None, session, None, None).expect("open app");
        app.open_workspace_root_input(false);
        app.workspace_root_input = extra.to_string_lossy().into_owned();
        app.apply_workspace_root_input().expect("add root");
        let extra = fs::canonicalize(extra).expect("canonical root");
        assert!(app.workspace_roots.contains(&extra));
        app.open_workspace_root_input(true);
        app.workspace_root_input = extra.to_string_lossy().into_owned();
        app.apply_workspace_root_input().expect("remove root");
        assert!(!app.workspace_roots.contains(&extra));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_sr02_workspace_search_runs_async_and_opens_result() {
        let root = std::env::temp_dir().join(format!("mtide-search-ui-{}", std::process::id()));
        fs::create_dir_all(root.join("node_modules")).expect("create search roots");
        fs::write(root.join(".gitignore"), "node_modules/\n").expect("write ignore rules");
        fs::write(root.join("main.rs"), "fn main() { needle(); }\n").expect("write source");
        fs::write(root.join("node_modules/ignored.rs"), "needle\n").expect("write ignored");
        let session = root.join("session.json");
        let mut app = App::open(None, false, None, session, None, None).expect("open app");
        app.workspace_roots.push(root.clone());
        app.open_workspace_search();
        app.workspace_search_query = "needle".into();
        app.start_workspace_search();
        for _ in 0..100 {
            app.poll_workspace_search();
            if app.workspace_search_job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.workspace_search_results.len(), 1);
        assert!(app.workspace_search_results[0].path.ends_with("main.rs"));
        app.open_workspace_search_result().expect("open result");
        assert_eq!(app.path.as_deref(), Some(root.join("main.rs").as_path()));
        assert_eq!(app.current_line(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_sr03_workspace_replace_rolls_back_as_one_snapshot() {
        let root = std::env::temp_dir().join(format!("mtide-replace-ui-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create replace root");
        let file = root.join("main.txt");
        fs::write(&file, "old old\n").expect("write replace file");
        let session = root.join("session.json");
        let mut app = App::open(None, false, None, session, None, None).expect("open app");
        app.workspace_roots.push(root.clone());
        app.workspace_search_query = "old".into();
        app.start_workspace_search();
        for _ in 0..100 {
            app.poll_workspace_search();
            if app.workspace_search_job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        app.workspace_replace_value = "new".into();
        app.start_workspace_replace();
        for _ in 0..100 {
            app.poll_workspace_replace();
            if app.workspace_replace_job.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            fs::read_to_string(&file).expect("read replaced"),
            "new new\n"
        );
        app.rollback_workspace_replace().expect("rollback replace");
        assert_eq!(
            fs::read_to_string(&file).expect("read rollback"),
            "old old\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_p1_workspace_open_edit_save_and_restart_recovery_round_trip() {
        let root = std::env::temp_dir().join(format!("mtide-e2e-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create workspace");
        let file = root.join("main.rs");
        let session = root.join(".mtide").join("session.json");
        fs::write(&file, "fn main() {}\n").expect("write source");
        let mut first = App::open(Some(file.clone()), false, None, session.clone(), None, None)
            .expect("open source");
        first.cursor = first.document.text().len().saturating_sub(1);
        first
            .document
            .apply(&[Edit {
                range: first.cursor..first.cursor,
                insert: "\n// edited".into(),
            }])
            .expect("edit source");
        first.save().expect("save source");
        first.persist_session().expect("persist session");
        assert!(fs::read_to_string(&file)
            .expect("read source")
            .contains("edited"));

        let mut restored =
            App::open(None, false, None, session, None, None).expect("restore session");
        assert_eq!(restored.path, Some(file.clone()));
        assert!(restored.document.text().contains("edited"));
        assert!(restored.cursor > 0);
        fs::write(&file, "fn main() { externally changed }\n").expect("external edit");
        restored.last_watch_poll =
            std::time::Instant::now() - std::time::Duration::from_millis(101);
        restored.poll_workspace_changes();
        assert_eq!(
            restored.document.text(),
            "fn main() { externally changed }\n"
        );
        assert!(!restored.external_change);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws03_restore_preserves_unsaved_snapshot_as_dirty() {
        let root = std::env::temp_dir().join(format!("mtide-dirty-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create dirty fixture");
        let file = root.join("main.rs");
        let session = root.join("session.json");
        fs::write(&file, "before\n").expect("write source");
        let mut app = App::open(Some(file.clone()), false, None, session.clone(), None, None)
            .expect("open source");
        app.document
            .apply(&[Edit {
                range: 0..0,
                insert: "unsaved\n".into(),
            }])
            .expect("edit source");
        app.persist_session().expect("persist dirty session");
        let restored = App::open(None, false, None, session, None, None).expect("restore session");
        assert!(restored.document.is_dirty());
        assert!(restored.document.text().starts_with("unsaved\n"));
        assert_eq!(
            restored.notification.as_deref(),
            Some("restored unsaved session changes")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed13_editor_save_preserves_crlf_line_endings() {
        let root = std::env::temp_dir().join(format!("mtide-crlf-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create test directory");
        let path = root.join("main.rs");
        fs::write(&path, b"a\r\nb\r\n").expect("write CRLF file");
        let mut app = App::open(
            Some(path.clone()),
            false,
            None,
            root.join("session.json"),
            None,
            None,
        )
        .expect("open CRLF file");
        let end = app.document.text().len();
        app.document
            .apply(&[Edit {
                range: end..end,
                insert: "c\n".into(),
            }])
            .expect("edit CRLF file");
        app.save().expect("save CRLF file");
        assert_eq!(
            fs::read(&path).expect("read saved file"),
            b"a\r\nb\r\nc\r\n"
        );
        let _ = fs::remove_dir_all(root);
    }
}
