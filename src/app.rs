use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::{
    fs,
    path::{Component, Path},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::{Settings, Theme},
    core::{Buffer, ExternalChange, Position},
    error::AppResult,
    git::{
        apply_hunk as git_apply_hunk, base_file as git_base_file, blame as git_blame,
        branches as git_branches, checkout as git_checkout, checkout_conflict_side,
        commit as git_commit, conflict_blocks, create_branch as git_create_branch,
        current_branch as git_branch, delete_branch as git_delete_branch,
        diff_refs as git_diff_refs, fetch as git_fetch,
        fetch_pull_request as git_fetch_pull_request, history_page as git_history_page,
        hunk_count as git_hunk_count, hunk_start_line,
        list_pull_request_comments as git_list_pr_comments, pull as git_pull, push as git_push,
        remote_with_credentials as git_remote_credentials, resolve_ref as git_resolve_ref,
        show_commit as git_show_commit, stage as git_stage, status as git_status, sync as git_sync,
        unstage as git_unstage, BlameLine, DiffLine, GitBackend, GitChange, GitCommit,
        PullRequestComment, VcsBackend,
    },
    lsp::{LspManager, LspServerState},
    plugin::{
        download_plugin_registry_entry, fetch_plugin_registry, install_plugin_archive,
        PluginDiffHunk, PluginDiffSnapshot, PluginEditorRequest, PluginEditorSnapshot,
        PluginEventArg, PluginPackage, PluginPanelLocation, PluginPanelRequest,
        PluginRegistryEntry, PluginRuntime,
    },
    search::{
        replace_first_with_options, replace_with_options, search_text_with_options,
        search_workspace_roots_with_options, search_workspace_roots_with_options_cancelled,
        SearchResult,
    },
    terminal::{TerminalState, TerminalStyle},
};

const AGENT_TOOL_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const AGENT_TOOL_MAX_CONCURRENT: usize = 4;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Explorer,
    Search,
    Changes,
    Branches,
    Editor,
    Terminal,
    Output,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginDialog {
    Confirm {
        runtime_index: usize,
        id: u64,
        message: String,
    },
    Info {
        title: String,
        entries: Vec<(String, String)>,
    },
    Custom {
        runtime_index: usize,
        id: u64,
        title: String,
        width: u16,
        height: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandLine {
    pub runtime_index: usize,
    pub id: u64,
    pub prefix: String,
    pub text: String,
    pub cursor: usize,
}

fn edit_overlay_text(
    text: &mut String,
    cursor: &mut usize,
    key_name: &str,
    rune: Option<char>,
) -> Option<bool> {
    let mut chars: Vec<char> = text.chars().collect();
    *cursor = (*cursor).min(chars.len());
    match key_name {
        "Left" => {
            *cursor = cursor.saturating_sub(1);
            Some(false)
        }
        "Right" => {
            *cursor = (*cursor + 1).min(chars.len());
            Some(false)
        }
        "Home" => {
            *cursor = 0;
            Some(false)
        }
        "End" => {
            *cursor = chars.len();
            Some(false)
        }
        "Backspace" => {
            if *cursor == 0 {
                return Some(false);
            }
            chars.remove(*cursor - 1);
            *cursor -= 1;
            *text = chars.into_iter().collect();
            Some(true)
        }
        "Delete" => {
            if *cursor >= chars.len() {
                return Some(false);
            }
            chars.remove(*cursor);
            *text = chars.into_iter().collect();
            Some(true)
        }
        "Char" => {
            let rune = rune?;
            chars.insert(*cursor, rune);
            *cursor += 1;
            *text = chars.into_iter().collect();
            Some(true)
        }
        _ => None,
    }
}

fn cursor_from_display_column(text: &str, column: usize) -> usize {
    let mut display_column = 0usize;
    for (index, character) in text.chars().enumerate() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if column < display_column.saturating_add(width.div_ceil(2)) {
            return index;
        }
        display_column = display_column.saturating_add(width);
    }
    text.chars().count()
}

fn edit_command_line(
    command_line: &mut PluginCommandLine,
    key_name: &str,
    rune: Option<char>,
) -> Option<bool> {
    let mut chars: Vec<char> = command_line.text.chars().collect();
    command_line.cursor = command_line.cursor.min(chars.len());
    match key_name {
        "Left" => {
            command_line.cursor = command_line.cursor.saturating_sub(1);
            Some(false)
        }
        "Right" => {
            command_line.cursor = (command_line.cursor + 1).min(chars.len());
            Some(false)
        }
        "Home" => {
            command_line.cursor = 0;
            Some(false)
        }
        "End" => {
            command_line.cursor = chars.len();
            Some(false)
        }
        "Backspace" => {
            if command_line.cursor == 0 {
                return Some(false);
            }
            chars.remove(command_line.cursor - 1);
            command_line.cursor -= 1;
            command_line.text = chars.into_iter().collect();
            Some(true)
        }
        "Delete" => {
            if command_line.cursor >= chars.len() {
                return Some(false);
            }
            chars.remove(command_line.cursor);
            command_line.text = chars.into_iter().collect();
            Some(true)
        }
        "Char" => {
            let rune = rune?;
            chars.insert(command_line.cursor, rune);
            command_line.cursor += 1;
            command_line.text = chars.into_iter().collect();
            Some(true)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginContextMenu {
    pub runtime_index: usize,
    pub items: Vec<String>,
    pub selected: usize,
    pub panel_id: Option<String>,
    pub panel_row: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCredentialPrompt {
    pub operation: String,
    pub root: PathBuf,
    pub username: String,
    pub password: String,
    pub username_cursor: usize,
    pub password_cursor: usize,
    pub password_focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeTreeRow {
    pub key: String,
    pub label: String,
    pub path: String,
    pub group: Option<ChangeGroup>,
    pub kind: ChangeTreeRowKind,
    pub depth: usize,
    pub file_index: Option<usize>,
    pub expanded: bool,
    pub descendant_files: usize,
    pub status: String,
    pub conflict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeGroup {
    Staged,
    Changes,
}

impl ChangeGroup {
    fn key(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Changes => "changes",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Staged => "Staged Changes",
            Self::Changes => "Changes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeTreeRowKind {
    Section,
    Directory,
    File,
}

#[derive(Default)]
struct ChangesTreeNode {
    directories: BTreeMap<String, ChangesTreeNode>,
    files: BTreeMap<String, usize>,
}

#[derive(Debug)]
struct GitRemoteCompletion {
    operation: String,
    root: PathBuf,
    result: Result<(), String>,
}

#[derive(Debug)]
struct GitBlameCompletion {
    generation: usize,
    root: PathBuf,
    relative: String,
    lines: Vec<BlameLine>,
}

#[derive(Debug)]
struct GitHistoryCompletion {
    generation: usize,
    root: PathBuf,
    commits: Vec<GitCommit>,
    snapshot_head: Option<String>,
    append: bool,
    has_more: bool,
}

#[derive(Debug)]
struct GitCommitDetailCompletion {
    generation: usize,
    root: PathBuf,
    hash: String,
    result: Result<String, String>,
}

#[derive(Debug)]
struct GitStateCompletion {
    generation: usize,
    active_root: PathBuf,
    changes: Vec<GitChange>,
    change_roots: Vec<PathBuf>,
    changes_by_root: BTreeMap<PathBuf, Vec<GitChange>>,
    branch: Option<String>,
    branches: Vec<String>,
    change_diff: Vec<DiffLine>,
}

#[derive(Debug)]
struct PullRequestReviewCompletion {
    generation: usize,
    root: PathBuf,
    repository: String,
    number: u64,
    result: Result<(String, Vec<DiffLine>), String>,
}

#[derive(Debug)]
struct PullRequestCommentsCompletion {
    generation: usize,
    repository: String,
    number: u64,
    result: Result<Vec<PullRequestComment>, String>,
}

#[derive(Debug)]
struct PullRequestCommentCompletion {
    generation: usize,
    repository: String,
    number: u64,
    result: Result<PullRequestComment, String>,
}

#[derive(Debug)]
struct PendingLspRequest {
    generation: usize,
    kind: PendingLspKind,
    path: PathBuf,
    deadline: Instant,
}

#[derive(Debug)]
enum PendingLspKind {
    Query(String),
    Rename {
        uri: String,
    },
    Edits {
        method: String,
        request_method: String,
        uri: String,
    },
    CompletionResolve {
        item: LspCompletionItem,
        commit_character: Option<char>,
    },
    ExecuteCommand {
        uri: String,
    },
}
impl Focus {
    pub const ALL: [Self; 8] = [
        Self::Explorer,
        Self::Search,
        Self::Changes,
        Self::Branches,
        Self::Editor,
        Self::Terminal,
        Self::Output,
        Self::Agent,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::Explorer => "Explorer",
            Self::Search => "Find",
            Self::Changes => "Changes",
            Self::Branches => "Branches",
            Self::Editor => "Editor",
            Self::Terminal => "Terminal",
            Self::Output => "Output",
            Self::Agent => "AI Agent",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Explorer,
    Search,
    Changes,
    Branches,
    Problems,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: usize,
    pub column: usize,
    pub severity: u8,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExplorerMode {
    CreateFile,
    CreateDirectory,
    Rename(PathBuf),
    ConfirmDelete(PathBuf),
}

impl ExplorerMode {
    const fn title(&self) -> &'static str {
        match self {
            Self::CreateFile => "New file",
            Self::CreateDirectory => "New directory",
            Self::Rename(_) => "Rename",
            Self::ConfirmDelete(_) => "Confirm delete",
        }
    }
}
impl SidebarTab {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Explorer => "Explore",
            Self::Search => "Find",
            Self::Changes => "Changes",
            Self::Branches => "Branches",
            Self::Problems => "Problems",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomTab {
    Terminal,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspPopup {
    Completion {
        items: Vec<LspCompletionItem>,
        selected: usize,
    },
    CodeActions {
        items: Vec<String>,
        edits: Vec<serde_json::Value>,
        selected: usize,
    },
    Locations {
        title: String,
        locations: Vec<(PathBuf, usize, usize)>,
        selected: usize,
    },
    Symbols {
        items: Vec<(String, PathBuf, usize, usize)>,
        selected: usize,
    },
    RenamePreview {
        uri: String,
        edit: serde_json::Value,
        summary: Vec<String>,
    },
    ConflictPreview {
        lines: Vec<String>,
    },
    Info(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspCompletionItem {
    pub(crate) label: String,
    pub(crate) insert_text: String,
    edit_range: Option<LspCompletionRange>,
    additional_edits: Vec<LspCompletionTextEdit>,
    commit_characters: Vec<char>,
    resolve_params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LspCompletionRange {
    start_line: usize,
    start_character: usize,
    end_line: usize,
    end_character: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LspCompletionTextEdit {
    range: LspCompletionRange,
    new_text: String,
}
impl BottomTab {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Output => "Output",
        }
    }
}

pub struct App {
    focus: Focus,
    sidebar_tab: SidebarTab,
    bottom_tab: BottomTab,
    sidebar_visible: bool,
    bottom_visible: bool,
    terminal_fullscreen: bool,
    fullscreen_sidebar_visible: bool,
    fullscreen_agent_visible: bool,
    agent_visible: bool,
    quit: bool,
    buffers: Vec<Buffer>,
    active_tab: usize,
    closed_tabs: Vec<ClosedTab>,
    search_query: String,
    search_include: String,
    search_exclude: String,
    search_field: u8,
    search_cursor: usize,
    replace_query: String,
    replace_cursor: usize,
    replace_mode: bool,
    search_regex: bool,
    search_results: Vec<SearchResult>,
    search_index: usize,
    search_rx: Receiver<SearchCompletion>,
    search_tx: Sender<SearchCompletion>,
    search_generation: Arc<AtomicUsize>,
    search_pending: bool,
    markdown_image_rx: Receiver<MarkdownImageCompletion>,
    markdown_image_tx: Sender<MarkdownImageCompletion>,
    markdown_image_pending: bool,
    search_root: PathBuf,
    workspace_roots: Vec<PathBuf>,
    explorer_roots: Vec<PathBuf>,
    explorer_paths: Vec<PathBuf>,
    explorer_expanded: BTreeSet<PathBuf>,
    explorer_index: usize,
    explorer_mode: Option<ExplorerMode>,
    explorer_input: String,
    explorer_cursor: usize,
    changes: Vec<GitChange>,
    change_roots: Vec<PathBuf>,
    git_changes_by_root: BTreeMap<PathBuf, Vec<GitChange>>,
    branch: Option<String>,
    branches: Vec<String>,
    branch_index: usize,
    branch_mode: bool,
    branch_name: String,
    branch_cursor: usize,
    commit_mode: bool,
    commit_message: String,
    commit_cursor: usize,
    change_index: usize,
    changes_tree_selection: Option<String>,
    changes_tree_collapsed: BTreeSet<String>,
    changes_tree_scroll: usize,
    change_hunk_index: usize,
    change_diff: Vec<DiffLine>,
    diff_split: bool,
    diff_full_context: bool,
    diff_expanded_context: BTreeSet<usize>,
    pull_request_target: Option<(String, u64)>,
    pull_request_head: Option<String>,
    pull_request_comments: Vec<PullRequestComment>,
    pull_request_comments_visible: bool,
    pull_request_comment_mode: bool,
    blame_lines: Vec<BlameLine>,
    history: Vec<GitCommit>,
    history_snapshot_head: Option<String>,
    history_root: Option<PathBuf>,
    history_offset: usize,
    history_has_more: bool,
    history_pending: bool,
    commit_detail_rx: Receiver<GitCommitDetailCompletion>,
    commit_detail_tx: Sender<GitCommitDetailCompletion>,
    commit_detail_generation: Arc<AtomicUsize>,
    commit_detail: Option<(GitCommit, PathBuf, String)>,
    commit_detail_pending: bool,
    output_scroll: usize,
    output_hover_link: Option<(usize, String)>,
    settings: Settings,
    auto_detect_indent: bool,
    last_autosave: Instant,
    settings_path: Option<PathBuf>,
    language_overrides: BTreeMap<String, String>,
    language_overrides_path: Option<PathBuf>,
    theme: Theme,
    theme_options: Vec<(String, Theme)>,
    theme_index: usize,
    terminals: Vec<TerminalState>,
    active_terminal: usize,
    agent: TerminalState,
    agent_provider: String,
    agent_session_path: Option<PathBuf>,
    agent_protocol_buffer: Vec<u8>,
    agent_tool_rx: Receiver<AgentToolCompletion>,
    agent_tool_tx: Sender<AgentToolCompletion>,
    agent_tool_inflight: Arc<AtomicUsize>,
    agent_tool_generation: Arc<AtomicUsize>,
    agent_install_confirm: bool,
    agent_install_pending: bool,
    status_message: String,
    plugin_runtimes: Vec<PluginRuntime>,
    plugin_registry_entries: Vec<PluginRegistryEntry>,
    plugin_drawer: Option<PluginDrawerState>,
    plugin_custom_tab: Option<(usize, String)>,
    plugin_tab_scroll: usize,
    plugin_tab_hunk: usize,
    plugin_tab_split: bool,
    plugin_tab_collapsed_hunks: BTreeSet<usize>,
    plugin_file_view: Option<(String, String)>,
    plugin_file_view_path: Option<PathBuf>,
    git_diff_tab_text: Option<String>,
    plugin_diff_file_path: Option<PathBuf>,
    plugin_file_close: Option<(usize, u64)>,
    plugin_dialog: Option<PluginDialog>,
    plugin_command_line: Option<PluginCommandLine>,
    plugin_context_menu: Option<PluginContextMenu>,
    git_credential_prompt: Option<GitCredentialPrompt>,
    git_remote_rx: Receiver<GitRemoteCompletion>,
    git_remote_tx: Sender<GitRemoteCompletion>,
    git_remote_pending: Option<String>,
    git_blame_rx: Receiver<GitBlameCompletion>,
    git_blame_tx: Sender<GitBlameCompletion>,
    git_blame_generation: Arc<AtomicUsize>,
    git_history_rx: Receiver<GitHistoryCompletion>,
    git_history_tx: Sender<GitHistoryCompletion>,
    git_history_generation: Arc<AtomicUsize>,
    git_state_rx: Receiver<GitStateCompletion>,
    git_state_tx: Sender<GitStateCompletion>,
    git_state_generation: Arc<AtomicUsize>,
    git_state_pending: bool,
    pull_request_review_rx: Receiver<PullRequestReviewCompletion>,
    pull_request_review_tx: Sender<PullRequestReviewCompletion>,
    pull_request_review_generation: Arc<AtomicUsize>,
    pull_request_comments_rx: Receiver<PullRequestCommentsCompletion>,
    pull_request_comments_tx: Sender<PullRequestCommentsCompletion>,
    pull_request_comments_generation: Arc<AtomicUsize>,
    pull_request_comment_rx: Receiver<PullRequestCommentCompletion>,
    pull_request_comment_tx: Sender<PullRequestCommentCompletion>,
    pull_request_comment_generation: Arc<AtomicUsize>,
    pull_request_comment_pending: bool,
    lsp: LspManager,
    lsp_pending: HashMap<(String, u64), PendingLspRequest>,
    lsp_generation: usize,
    lsp_popup: Option<LspPopup>,
    completion_due: Option<Instant>,
    ai_rx: Receiver<AiCompletion>,
    ai_tx: Sender<AiCompletion>,
    ai_generation: Arc<AtomicUsize>,
    format_on_save_pending: bool,
    rename_mode: bool,
    rename_text: String,
    plugin_palette: bool,
    plugin_query: String,
    plugin_index: usize,
    key_chord_prefix: bool,
    settings_overlay: bool,
    settings_index: usize,
    overlay_focus: Option<Focus>,
    quick_open: bool,
    quick_open_query: String,
    quick_open_cursor: usize,
    quick_open_save_as: bool,
    quick_open_save_workspace: bool,
    quick_open_add_folder: bool,
    quick_open_results: Vec<PathBuf>,
    quick_open_index: usize,
    goto_mode: bool,
    goto_input: String,
    goto_cursor: usize,
    selection_anchor: Option<Position>,
    clipboard: String,
    osc52_clipboard: Option<String>,
    horizontal_scroll: usize,
    editor_viewport_width: Cell<usize>,
    automation_size: Cell<(u16, u16)>,
    editor_scroll_line: Option<usize>,
    editor_viewport_height: Cell<usize>,
    terminal_viewport_rows: Cell<usize>,
    secondary_cursors: Vec<Position>,
    secondary_selections: Vec<(Position, Position)>,
    column_selection: Option<(Position, Position)>,
    mouse_selecting: bool,
    editor_hover_link: Option<(usize, String)>,
    plugin_view_hover_link: Option<(usize, String)>,
    folded_lines: BTreeSet<usize>,
    diff_base_lines: Option<Vec<String>>,
    plugin_mouse_events: Vec<MouseEvent>,
    terminal_selection: Option<(TerminalPoint, TerminalPoint)>,
    terminal_selection_dragging: bool,
    sidebar_resizing: bool,
    bottom_resizing: bool,
    pending_automation_waits: Vec<PendingAutomationWait>,
    plugin_sidebar_line_start: Cell<Option<usize>>,
    plugin_bottom_line_start: Cell<Option<usize>>,
    plugin_status_hits: RefCell<Vec<PluginStatusHit>>,
    problem_index: usize,
}

#[derive(Debug)]
struct ClosedTab {
    buffer: Buffer,
    selection_anchor: Option<Position>,
    secondary_cursors: Vec<Position>,
    secondary_selections: Vec<(Position, Position)>,
    column_selection: Option<(Position, Position)>,
    horizontal_scroll: usize,
    editor_scroll_line: Option<usize>,
    folded_lines: BTreeSet<usize>,
    diff_base_lines: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginDrawerState {
    runtime_index: usize,
    side: String,
    width: u16,
    min_width: u16,
}

#[derive(Debug, Clone)]
struct PluginStatusHit {
    start: u16,
    end: u16,
    runtime_index: usize,
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TerminalPoint {
    column: u16,
    row: u16,
}

#[derive(Debug)]
struct PendingAutomationWait {
    runtime_index: usize,
    id: u64,
    needle: String,
    deadline: Instant,
}

#[derive(Debug)]
struct AgentToolCompletion {
    generation: usize,
    response: JsonValue,
}

#[derive(Debug)]
struct SearchCompletion {
    generation: usize,
    results: Vec<SearchResult>,
}

#[derive(Debug)]
struct AiCompletion {
    generation: usize,
    path: PathBuf,
    cursor: Position,
    result: Result<String, String>,
}

#[derive(Debug)]
struct MarkdownImageCompletion {
    document: PathBuf,
    bytes: Option<Vec<u8>>,
}

type PluginStatusEntry = (usize, String, String);
impl App {
    pub fn new(args: &[String]) -> AppResult<Self> {
        Self::new_with_stdin(args, None)
    }

    pub fn new_with_stdin(args: &[String], stdin_text: Option<&str>) -> AppResult<Self> {
        let explicit_target = args
            .windows(2)
            .find(|pair| pair[0] == "--workspace")
            .map(|pair| pair[1].clone())
            .or_else(|| {
                args.iter()
                    .enumerate()
                    .find_map(|(index, arg)| {
                        if (arg.starts_with('-') && arg != "-")
                            || (index > 0
                                && args.get(index - 1).is_some_and(|previous| {
                                    matches!(
                                        previous.as_str(),
                                        "--plugin"
                                            | "--config"
                                            | "--workspace"
                                            | "--exec"
                                            | "--exec-split-on"
                                            | "--size"
                                    )
                                }))
                        {
                            None
                        } else {
                            Some(arg)
                        }
                    })
                    .cloned()
            });
        let settings_path = args
            .windows(2)
            .find(|pair| pair[0] == "--config")
            .map(|pair| PathBuf::from(&pair[1]));
        let mut settings = args
            .windows(2)
            .find(|pair| pair[0] == "--config")
            .map(|pair| Settings::load_from_path(&pair[1]))
            .unwrap_or_else(Settings::load);
        if args.iter().any(|arg| arg == "--clear-default-folders") {
            settings.default_folders.clear();
            if let Some(path) = settings_path.as_ref() {
                let _ = settings.save_to_path(path);
            } else {
                let _ = settings.save();
            }
        }
        let welcome = args.iter().any(|arg| arg == "--welcome");
        let using_default_folders = explicit_target.is_none() && !welcome && stdin_text.is_none();
        let pull_request = explicit_target
            .as_deref()
            .and_then(parse_github_pull_request_url);
        let target = if pull_request.is_some() {
            None
        } else {
            explicit_target
                .as_deref()
                .map(parse_location_target)
                .or_else(|| {
                    (!welcome)
                        .then(|| settings.default_folders.first().map(String::as_str))
                        .flatten()
                        .map(parse_location_target)
                })
        };
        let target = if !welcome && target.is_none() && stdin_text.is_some() {
            Some(parse_location_target("-"))
        } else {
            target
        };
        let target_path = target.as_ref().map(|target| target.path.clone());
        let mut startup_error = None;
        let mut buffer = match target_path.as_deref() {
            Some(path) if path == Path::new("-") => {
                Buffer::from_text(stdin_text.unwrap_or_default())
            }
            Some(path)
                if path.extension().and_then(|extension| extension.to_str()) != Some("ttt")
                    && !path.is_dir() =>
            {
                match Buffer::open(path) {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        startup_error =
                            Some(format!("Open failed for {}: {error}", path.display()));
                        Buffer::welcome()
                    }
                }
            }
            _ => Buffer::welcome(),
        };
        if let Some(target) = &target {
            if target.path.is_file()
                && target
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("ttt")
            {
                buffer.set_cursor(target.line, target.column);
            }
        }
        let lsp = LspManager::from_settings(&settings.lsp);
        let agent_session_path = agent_session_path();
        let saved_agent_session = agent_session_path.as_deref().and_then(load_agent_session);
        let agent_provider = select_agent_provider(&settings, saved_agent_session.as_ref());
        let (agent_command, agent_cwd, agent_persistent) =
            configured_agent_command(&settings, &agent_provider);
        let agent = TerminalState::with_command(agent_command, agent_cwd);
        let mut agent = agent;
        if let Some(session) = saved_agent_session
            .as_ref()
            .filter(|session| session.provider == agent_provider && !agent_persistent)
        {
            agent.append_input(&session.input);
            agent.restore_screen(&session.screen);
        }
        let mut terminal = TerminalState::with_command(terminal_command(&settings), None);
        terminal.set_scrollback(settings.terminal.scrollback);
        let workspace_roots = match if using_default_folders {
            default_workspace_roots(&settings)
        } else {
            workspace_roots(target.as_ref())
        } {
            Ok(roots) => roots,
            Err(error) => {
                if startup_error.is_none() {
                    startup_error = Some(format!("Workspace open failed: {error}"));
                }
                vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
            }
        };
        let search_root = workspace_roots
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let explorer_roots = target
            .as_ref()
            .filter(|target| {
                target
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("ttt")
            })
            .and_then(|target| {
                fs::read_to_string(&target.path)
                    .ok()
                    .and_then(|contents| serde_json::from_str::<WorkspaceFile>(&contents).ok())
                    .map(|workspace| {
                        let base = target.path.parent().unwrap_or_else(|| Path::new("."));
                        workspace
                            .folders
                            .into_iter()
                            .map(|folder| {
                                if folder.path.is_absolute() {
                                    folder.path
                                } else {
                                    base.join(folder.path)
                                }
                            })
                            .collect::<Vec<_>>()
                    })
            })
            .filter(|roots| !roots.is_empty())
            .unwrap_or_else(|| workspace_roots.clone());
        let language_overrides_path = language_overrides_path(&search_root);
        let language_overrides = language_overrides_path
            .as_deref()
            .and_then(load_language_overrides)
            .unwrap_or_default();
        if let Some(path) = buffer.path().map(Path::to_path_buf) {
            if let Some(language) = language_overrides.get(&path_key(&path)) {
                buffer.set_language_override(Some(language.clone()));
            }
        }
        let language = buffer
            .path()
            .and_then(|path| path.extension())
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_owned();
        let editorconfig_present = settings.apply_editorconfig(&search_root, &language);
        let auto_detect_indent = settings.editor.auto_detect_indent && !editorconfig_present;
        let explicit_plugins: Vec<_> = args
            .windows(2)
            .filter(|pair| pair[0] == "--plugin")
            .map(|pair| PluginPackage::load_argument(&pair[1]))
            .collect::<AppResult<Vec<_>>>()?;
        let mut plugins = explicit_plugins;
        let explicit_names = plugins
            .iter()
            .map(|package| package.manifest.name.clone())
            .collect::<BTreeSet<_>>();
        let discovered = discover_plugin_packages(&workspace_roots);
        for package in discovered {
            if !explicit_names.contains(&package.manifest.name)
                && !plugins
                    .iter()
                    .any(|existing| existing.manifest.name == package.manifest.name)
            {
                plugins.push(package);
            }
        }
        let plugin_runtimes: Vec<_> = plugins
            .iter()
            .filter(|package| package.manifest.entry.is_some())
            .map(|package| {
                load_plugin_runtime_for_workspace(
                    package,
                    settings_path.as_deref(),
                    &workspace_roots,
                )
            })
            .collect::<AppResult<Vec<_>>>()?;
        let plugin_log_count: usize = plugin_runtimes
            .iter()
            .map(|runtime| runtime.logs().len())
            .sum();
        let plugin_theme_count: usize = plugins
            .iter()
            .map(|package| package.theme_paths().len())
            .sum();
        let mut theme_options = vec![("Built-in".to_owned(), Theme::default())];
        if let Some(path) = settings.theme.as_ref() {
            let direct_path = PathBuf::from(path);
            let resolved_path = if direct_path.is_file() {
                Some(direct_path)
            } else {
                config_root(settings_path.as_deref())
                    .map(|root| root.join("themes").join(format!("{path}.json")))
                    .filter(|candidate| candidate.is_file())
            };
            if let Some(resolved_path) = resolved_path {
                if let Ok(theme) = Theme::from_path(&resolved_path) {
                    theme_options.push((path.clone(), theme));
                }
            }
        }
        for path in plugins.iter().flat_map(PluginPackage::theme_paths) {
            if let Ok(theme) = Theme::from_path(&path) {
                theme_options.push((path.display().to_string(), theme));
            }
        }
        let theme_index = if theme_options.len() > 1 { 1 } else { 0 };
        let theme = theme_options[theme_index].1;
        let (changes, change_roots) = workspace_git_changes(&workspace_roots);
        let git_changes_by_root = workspace_roots
            .iter()
            .map(|root| (root.clone(), git_status(root)))
            .collect();
        let branch = git_branch(&search_root);
        let branches = git_branches(&search_root);
        let change_diff = changes.first().map_or_else(Vec::new, |change| {
            GitBackend::new(&search_root).diff(change)
        });
        let explorer_paths = explorer_roots
            .iter()
            .flat_map(|root| {
                visible_explorer_entries(
                    root,
                    &BTreeSet::new(),
                    settings.explorer.show_hidden,
                    settings.explorer.show_git_ignored,
                )
            })
            .collect();
        let status_message = startup_error.unwrap_or_else(|| {
            plugins.first().map_or_else(
                || "Ready · Rust foundation".to_owned(),
                |package| {
                    let entry_ready = package
                        .entry_path()
                        .map(|path| path.is_file())
                        .unwrap_or(true);
                    format!(
                        "Loaded {} plugin(s) · {} · {} · {} logs · {} theme(s)",
                        plugins.len(),
                        package.manifest.title(),
                        if entry_ready {
                            "ready"
                        } else {
                            "entry missing"
                        },
                        plugin_log_count,
                        plugin_theme_count
                    )
                },
            )
        });
        let (git_remote_tx, git_remote_rx) = mpsc::channel();
        let (git_blame_tx, git_blame_rx) = mpsc::channel();
        let (git_history_tx, git_history_rx) = mpsc::channel();
        let (commit_detail_tx, commit_detail_rx) = mpsc::channel();
        let (git_state_tx, git_state_rx) = mpsc::channel();
        let (pull_request_review_tx, pull_request_review_rx) = mpsc::channel();
        let (pull_request_comments_tx, pull_request_comments_rx) = mpsc::channel();
        let (pull_request_comment_tx, pull_request_comment_rx) = mpsc::channel();
        let (agent_tool_tx, agent_tool_rx) = mpsc::channel();
        let (search_tx, search_rx) = mpsc::channel();
        let (ai_tx, ai_rx) = mpsc::channel();
        let (markdown_image_tx, markdown_image_rx) = mpsc::channel();
        let mut app = Self {
            focus: Focus::Editor,
            sidebar_tab: SidebarTab::Explorer,
            bottom_tab: BottomTab::Terminal,
            sidebar_visible: settings.sidebar_visible,
            bottom_visible: settings.bottom_visible,
            terminal_fullscreen: false,
            fullscreen_sidebar_visible: false,
            fullscreen_agent_visible: false,
            agent_visible: settings.agent_visible,
            quit: false,
            buffers: vec![buffer],
            active_tab: 0,
            closed_tabs: Vec::new(),
            search_query: String::new(),
            search_include: settings.search.include.join(","),
            search_exclude: settings.search.exclude.join(","),
            search_field: 0,
            search_cursor: 0,
            replace_query: String::new(),
            replace_cursor: 0,
            replace_mode: false,
            search_regex: false,
            search_results: Vec::new(),
            search_index: 0,
            search_rx,
            search_tx,
            search_generation: Arc::new(AtomicUsize::new(0)),
            search_pending: false,
            markdown_image_rx,
            markdown_image_tx,
            markdown_image_pending: false,
            search_root,
            workspace_roots,
            explorer_roots,
            explorer_paths,
            explorer_expanded: BTreeSet::new(),
            explorer_index: 0,
            explorer_mode: None,
            explorer_input: String::new(),
            explorer_cursor: 0,
            changes,
            change_roots,
            git_changes_by_root,
            branch,
            branches,
            branch_index: 0,
            branch_mode: false,
            branch_name: String::new(),
            branch_cursor: 0,
            commit_mode: false,
            commit_message: String::new(),
            commit_cursor: 0,
            change_index: 0,
            changes_tree_selection: None,
            changes_tree_collapsed: BTreeSet::new(),
            changes_tree_scroll: 0,
            change_hunk_index: 0,
            change_diff,
            diff_split: false,
            diff_full_context: settings.editor.diff_context.eq_ignore_ascii_case("full"),
            diff_expanded_context: BTreeSet::new(),
            pull_request_target: None,
            pull_request_head: None,
            pull_request_comments: Vec::new(),
            pull_request_comments_visible: false,
            pull_request_comment_mode: false,
            blame_lines: Vec::new(),
            history: Vec::new(),
            history_snapshot_head: None,
            history_root: None,
            history_offset: 0,
            history_has_more: false,
            history_pending: false,
            commit_detail_rx,
            commit_detail_tx,
            commit_detail_generation: Arc::new(AtomicUsize::new(0)),
            commit_detail: None,
            commit_detail_pending: false,
            output_scroll: 0,
            output_hover_link: None,
            settings,
            auto_detect_indent,
            last_autosave: Instant::now(),
            settings_path,
            language_overrides,
            language_overrides_path,
            theme,
            theme_options,
            theme_index,
            terminals: vec![terminal],
            active_terminal: 0,
            agent,
            agent_provider,
            agent_session_path,
            agent_protocol_buffer: Vec::new(),
            agent_tool_rx,
            agent_tool_tx,
            agent_tool_inflight: Arc::new(AtomicUsize::new(0)),
            agent_tool_generation: Arc::new(AtomicUsize::new(0)),
            agent_install_confirm: false,
            agent_install_pending: false,
            status_message,
            plugin_runtimes,
            plugin_registry_entries: Vec::new(),
            plugin_drawer: None,
            plugin_custom_tab: None,
            plugin_tab_scroll: 0,
            plugin_tab_hunk: 0,
            plugin_tab_split: false,
            plugin_tab_collapsed_hunks: BTreeSet::new(),
            plugin_file_view: None,
            plugin_file_view_path: None,
            git_diff_tab_text: None,
            plugin_diff_file_path: None,
            plugin_file_close: None,
            plugin_dialog: None,
            plugin_command_line: None,
            plugin_context_menu: None,
            git_credential_prompt: None,
            git_remote_rx,
            git_remote_tx,
            git_remote_pending: None,
            git_blame_rx,
            git_blame_tx,
            git_blame_generation: Arc::new(AtomicUsize::new(0)),
            git_history_rx,
            git_history_tx,
            git_history_generation: Arc::new(AtomicUsize::new(0)),
            git_state_rx,
            git_state_tx,
            git_state_generation: Arc::new(AtomicUsize::new(0)),
            git_state_pending: false,
            pull_request_review_rx,
            pull_request_review_tx,
            pull_request_review_generation: Arc::new(AtomicUsize::new(0)),
            pull_request_comments_rx,
            pull_request_comments_tx,
            pull_request_comments_generation: Arc::new(AtomicUsize::new(0)),
            pull_request_comment_rx,
            pull_request_comment_tx,
            pull_request_comment_generation: Arc::new(AtomicUsize::new(0)),
            pull_request_comment_pending: false,
            lsp,
            lsp_pending: HashMap::new(),
            lsp_generation: 0,
            lsp_popup: None,
            completion_due: None,
            ai_rx,
            ai_tx,
            ai_generation: Arc::new(AtomicUsize::new(0)),
            format_on_save_pending: false,
            rename_mode: false,
            rename_text: String::new(),
            plugin_palette: false,
            plugin_query: String::new(),
            plugin_index: 0,
            key_chord_prefix: false,
            settings_overlay: false,
            settings_index: 0,
            overlay_focus: None,
            quick_open: false,
            quick_open_query: String::new(),
            quick_open_cursor: 0,
            quick_open_save_as: false,
            quick_open_save_workspace: false,
            quick_open_add_folder: false,
            quick_open_results: Vec::new(),
            quick_open_index: 0,
            goto_mode: false,
            goto_input: String::new(),
            goto_cursor: 0,
            selection_anchor: None,
            clipboard: String::new(),
            osc52_clipboard: None,
            horizontal_scroll: 0,
            editor_viewport_width: Cell::new(0),
            automation_size: Cell::new((80, 24)),
            editor_scroll_line: None,
            editor_viewport_height: Cell::new(0),
            terminal_viewport_rows: Cell::new(3),
            secondary_cursors: Vec::new(),
            secondary_selections: Vec::new(),
            column_selection: None,
            mouse_selecting: false,
            editor_hover_link: None,
            plugin_view_hover_link: None,
            folded_lines: BTreeSet::new(),
            diff_base_lines: None,
            plugin_mouse_events: Vec::new(),
            terminal_selection: None,
            terminal_selection_dragging: false,
            sidebar_resizing: false,
            bottom_resizing: false,
            pending_automation_waits: Vec::new(),
            plugin_sidebar_line_start: Cell::new(None),
            plugin_bottom_line_start: Cell::new(None),
            plugin_status_hits: RefCell::new(Vec::new()),
            problem_index: 0,
        };
        if let Some(pull_request) = pull_request {
            if let Err(error) =
                app.load_pull_request_review(&pull_request.repository, pull_request.number)
            {
                app.status_message = format!("PR review failed: {error}");
            }
        }
        app.sync_lsp_for_buffer();
        app.emit_plugin_event("app.start");
        Ok(app)
    }
    pub const fn focus(&self) -> Focus {
        self.focus
    }
    pub fn set_focus(&mut self, focus: Focus) {
        if focus != Focus::Editor {
            self.editor_hover_link = None;
            self.plugin_view_hover_link = None;
        }
        if focus != Focus::Output {
            self.output_hover_link = None;
        }
        self.focus = focus;
    }
    pub fn automation_panel(&mut self, panel: &str) -> AppResult<()> {
        self.focus = match panel.to_ascii_lowercase().as_str() {
            "explorer" => Focus::Explorer,
            "find" | "search" => Focus::Search,
            "changes" => Focus::Changes,
            "branches" => Focus::Branches,
            "editor" => Focus::Editor,
            "terminal" => Focus::Terminal,
            "output" => Focus::Output,
            "agent" => Focus::Agent,
            other => {
                return Err(crate::error::AppError::Workspace(format!(
                    "unknown --exec panel: {other}"
                )))
            }
        };
        Ok(())
    }
    pub fn automation_command(&mut self, query: &str) -> AppResult<()> {
        let query = query.trim();
        let Some((id, _)) = self
            .command_palette_items("")
            .into_iter()
            .find(|(id, title)| {
                id.eq_ignore_ascii_case(query) || title.eq_ignore_ascii_case(query)
            })
        else {
            return Err(crate::error::AppError::Workspace(format!(
                "unknown --exec command: {query}"
            )));
        };
        if !self.execute_builtin_command(&id)? && !self.execute_plugin_command(&id)? {
            return Err(crate::error::AppError::Workspace(format!(
                "command is unavailable: {query}"
            )));
        }
        Ok(())
    }
    pub fn automation_open(
        &mut self,
        path: &str,
        line: Option<usize>,
        column: Option<usize>,
        readonly: bool,
    ) -> AppResult<()> {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.search_root.join(path)
        };
        let mut buffer = Buffer::open(&path)?;
        buffer.set_readonly(readonly);
        self.open_buffer(buffer);
        if let Some(line) = line {
            let line = line
                .saturating_sub(1)
                .min(self.file_lines().len().saturating_sub(1));
            let column = column.unwrap_or(1).saturating_sub(1);
            self.buffer_mut().set_cursor(line, column);
        }
        self.focus = Focus::Editor;
        Ok(())
    }
    pub const fn sidebar_tab(&self) -> SidebarTab {
        self.sidebar_tab
    }
    pub const fn bottom_tab(&self) -> BottomTab {
        self.bottom_tab
    }
    pub const fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }
    pub const fn bottom_visible(&self) -> bool {
        self.bottom_visible
    }
    pub const fn terminal_fullscreen(&self) -> bool {
        self.terminal_fullscreen
    }
    pub const fn agent_visible(&self) -> bool {
        self.agent_visible
    }
    pub const fn sidebar_width(&self) -> u16 {
        self.settings.sidebar_width
    }
    pub const fn bottom_height(&self) -> u16 {
        self.settings.bottom_height
    }
    pub const fn agent_width(&self) -> u16 {
        self.settings.agent_width
    }

    pub const fn sidebar_resizing(&self) -> bool {
        self.sidebar_resizing
    }

    pub fn begin_sidebar_resize(&mut self, width: u16, max_width: u16) {
        self.sidebar_resizing = true;
        self.resize_sidebar_to(width, max_width);
    }

    pub fn resize_sidebar_to(&mut self, width: u16, max_width: u16) {
        let next = width.clamp(20, max_width.clamp(20, 80));
        if next == self.settings.sidebar_width {
            return;
        }
        self.settings.sidebar_width = next;
        self.status_message = format!("Sidebar width: {next}");
    }

    pub fn end_sidebar_resize(&mut self) {
        self.sidebar_resizing = false;
    }

    pub const fn bottom_resizing(&self) -> bool {
        self.bottom_resizing
    }

    pub fn begin_bottom_resize(&mut self, height: u16) {
        self.bottom_resizing = true;
        self.set_bottom_height(height);
    }

    pub fn set_bottom_height(&mut self, height: u16) {
        let next = height.clamp(3, 24);
        if next == self.settings.bottom_height {
            return;
        }
        self.settings.bottom_height = next;
        self.status_message = format!("Bottom panel height: {next}");
    }

    pub fn end_bottom_resize(&mut self) {
        self.bottom_resizing = false;
    }

    pub fn resize_bottom_panel(&mut self, delta: i16) {
        let current = i32::from(self.settings.bottom_height);
        self.set_bottom_height((current + i32::from(delta)).clamp(3, 24) as u16);
    }
    pub fn diagnostic_severity(&self, line: usize) -> Option<u8> {
        let path = self.buffer().path()?;
        let path_text = path.to_string_lossy().into_owned();
        let uri = path_to_uri(path);
        let lsp_severity = self
            .lsp
            .diagnostics(&uri)
            .iter()
            .filter(|diagnostic| {
                (diagnostic.start.line as usize..=diagnostic.end.line as usize).contains(&line)
            })
            .map(|diagnostic| diagnostic.severity.unwrap_or(3))
            .min();
        let plugin_severity = self
            .plugin_runtimes
            .iter()
            .filter_map(|runtime| runtime.diagnostic_severity(&path_text, line))
            .min();
        lsp_severity.into_iter().chain(plugin_severity).min()
    }
    pub fn diagnostic_message(&self, line: usize) -> Option<String> {
        let path = self.buffer().path()?;
        let path_text = path.to_string_lossy().into_owned();
        let uri = path_to_uri(path);
        if let Some(message) = self
            .lsp
            .diagnostics(&uri)
            .iter()
            .find(|diagnostic| {
                (diagnostic.start.line as usize..=diagnostic.end.line as usize).contains(&line)
            })
            .map(|diagnostic| diagnostic.message.clone())
        {
            return Some(message);
        }
        self.plugin_runtimes
            .iter()
            .find_map(|runtime| runtime.diagnostic_message(&path_text, line))
    }
    pub fn is_diagnostic_position(&self, line: usize, column: usize) -> bool {
        let Some(path) = self.buffer().path() else {
            return false;
        };
        let line_text = self
            .file_lines()
            .get(line)
            .map(String::as_str)
            .unwrap_or("");
        let uri = path_to_uri(path);
        if self.lsp.diagnostics(&uri).iter().any(|diagnostic| {
            if !(diagnostic.start.line as usize..=diagnostic.end.line as usize).contains(&line) {
                return false;
            }
            let start = if line == diagnostic.start.line as usize {
                editor_column_from_utf16(line_text, diagnostic.start.character as usize)
                    .unwrap_or_else(|| line_text.chars().count())
            } else {
                0
            };
            let end = if line == diagnostic.end.line as usize {
                editor_column_from_utf16(line_text, diagnostic.end.character as usize)
                    .unwrap_or_else(|| line_text.chars().count())
            } else {
                line_text.chars().count()
            };
            (start..end).contains(&column)
        }) {
            return true;
        }
        let path_text = path.to_string_lossy().into_owned();
        self.plugin_runtimes.iter().any(|runtime| {
            runtime
                .diagnostics_for_path(&path_text)
                .iter()
                .any(|diagnostic| {
                    if !(diagnostic.line..=diagnostic.end_line).contains(&line) {
                        return false;
                    }
                    let start = if line == diagnostic.line {
                        diagnostic.col
                    } else {
                        0
                    };
                    let end = if line == diagnostic.end_line {
                        diagnostic.end_col
                    } else {
                        line_text.chars().count()
                    };
                    (start..end).contains(&column)
                })
        })
    }
    pub fn problems(&self) -> Vec<Problem> {
        let Some(path) = self.buffer().path() else {
            return Vec::new();
        };
        let uri = path_to_uri(path);
        let mut problems = self
            .lsp
            .diagnostics(&uri)
            .iter()
            .map(|diagnostic| Problem {
                line: diagnostic.start.line as usize,
                column: diagnostic.start.character as usize,
                severity: diagnostic.severity.unwrap_or(3),
                message: diagnostic.message.clone(),
            })
            .collect::<Vec<_>>();
        let path_text = path.to_string_lossy().into_owned();
        for runtime in &self.plugin_runtimes {
            problems.extend(
                runtime
                    .diagnostics_for_path(&path_text)
                    .into_iter()
                    .map(|item| Problem {
                        line: item.line,
                        column: item.col,
                        severity: item.severity,
                        message: item.message,
                    }),
            );
        }
        problems.sort_by_key(|problem| (problem.line, problem.column, problem.severity));
        problems
    }
    pub const fn problem_index(&self) -> usize {
        self.problem_index
    }
    pub fn set_problem_index(&mut self, index: usize) {
        self.problem_index = index.min(self.problems().len().saturating_sub(1));
    }
    pub fn activate_problem(&mut self, index: usize) {
        let problems = self.problems();
        let Some(problem) = problems.get(index) else {
            return;
        };
        self.problem_index = index;
        self.buffer_mut().set_cursor(problem.line, problem.column);
        self.editor_scroll_line = Some(problem.line);
        self.focus = Focus::Editor;
        self.status_message = format!(
            "Problem {}:{} · {}",
            problem.line + 1,
            problem.column + 1,
            problem.message
        );
    }
    pub fn decoration_style(&self, line: usize) -> Option<String> {
        let path = self.buffer().path()?.to_string_lossy().into_owned();
        self.plugin_runtimes
            .iter()
            .find_map(|runtime| runtime.decoration_style(&path, line))
    }
    pub fn bookmark_marker(&self, line: usize) -> Option<String> {
        let path = self.buffer().path()?.to_string_lossy().into_owned();
        self.plugin_runtimes
            .iter()
            .find_map(|runtime| runtime.bookmark_marker(&path, line))
    }
    pub fn file_lines(&self) -> &[String] {
        self.buffer().lines()
    }
    pub fn file_name(&self) -> &str {
        self.buffer().file_name()
    }
    pub fn file_path_label(&self) -> String {
        let Some(path) = self.buffer().path() else {
            return self.file_name().to_owned();
        };
        self.workspace_roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
    }
    pub fn diff_line_markers(&self) -> Vec<Option<char>> {
        self.diff_base_lines
            .as_deref()
            .map_or_else(Vec::new, |base| {
                editor_diff_markers(base, self.file_lines())
            })
    }
    pub const fn diff_mode(&self) -> bool {
        self.diff_base_lines.is_some()
    }
    pub fn language(&self) -> &str {
        if let Some(language) = self.buffer().language_override() {
            return language;
        }
        if let Some(language) = self.settings.editor.language.as_deref() {
            return language;
        }
        self.buffer()
            .path()
            .and_then(|path| path.extension())
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
    }
    pub fn indent_summary(&self) -> String {
        let mode = if self.effective_use_tabs() {
            "Tabs".to_owned()
        } else {
            format!("Spaces:{}", self.effective_indent_size())
        };
        if self.auto_detect_indent {
            format!("{mode}·auto")
        } else {
            mode
        }
    }
    fn cycle_language_override(&mut self) {
        const LANGUAGES: &[&str] = &[
            "rust", "go", "py", "js", "ts", "json", "md", "toml", "yaml", "sh", "lua",
        ];
        let current = self.language().to_owned();
        let next = self
            .buffer()
            .language_override()
            .and_then(|language| {
                LANGUAGES
                    .iter()
                    .position(|candidate| *candidate == language)
                    .map(|index| (index + 1) % (LANGUAGES.len() + 1))
            })
            .unwrap_or_else(|| {
                LANGUAGES
                    .iter()
                    .position(|candidate| *candidate == current)
                    .map(|index| index + 1)
                    .unwrap_or(0)
            });
        if next == LANGUAGES.len() {
            self.buffer_mut().set_language_override(None);
            self.status_message = "Language: auto".to_owned();
        } else {
            let language = LANGUAGES[next].to_owned();
            self.buffer_mut()
                .set_language_override(Some(language.clone()));
            self.status_message = format!("Language: {language}");
        }
        if let Some(path) = self.buffer().path().map(Path::to_path_buf) {
            let key = path_key(&path);
            if let Some(language) = self.buffer().language_override() {
                self.language_overrides.insert(key, language.to_owned());
            } else {
                self.language_overrides.remove(&key);
            }
            self.save_language_overrides();
        }
        self.sync_lsp_for_buffer();
    }
    pub fn cursor(&self) -> Position {
        self.buffer().cursor()
    }
    pub fn is_line_folded(&self, line: usize) -> bool {
        self.folded_lines.contains(&line)
    }
    pub fn is_line_foldable(&self, line: usize) -> bool {
        self.fold_end(line).is_some()
    }
    pub fn is_line_hidden(&self, line: usize) -> bool {
        self.folded_lines
            .iter()
            .any(|start| *start < line && self.fold_end(*start).is_some_and(|end| line < end))
    }
    pub fn selection_lines(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let cursor = self.buffer().cursor();
        Some((anchor.line.min(cursor.line), anchor.line.max(cursor.line)))
    }
    pub fn is_line_selected(&self, line: usize) -> bool {
        self.selection_lines()
            .is_some_and(|(start, end)| (start..=end).contains(&line))
            || self.column_selection.is_some_and(|(first, second)| {
                (first.line.min(second.line)..=first.line.max(second.line)).contains(&line)
            })
            || self.secondary_selections.iter().any(|(first, second)| {
                let start = first.line.min(second.line);
                let end = first.line.max(second.line);
                (start..=end).contains(&line)
            })
    }
    pub fn is_position_selected(&self, line: usize, column: usize) -> bool {
        fn contains((first, second): (Position, Position), line: usize, column: usize) -> bool {
            let (start, end) = if (first.line, first.column) <= (second.line, second.column) {
                (first, second)
            } else {
                (second, first)
            };
            if line < start.line || line > end.line {
                return false;
            }
            if start.line == end.line {
                return line == start.line && (start.column..end.column).contains(&column);
            }
            if line == start.line {
                column >= start.column
            } else if line == end.line {
                column < end.column
            } else {
                true
            }
        }

        self.active_selection_ranges()
            .into_iter()
            .any(|range| contains(range, line, column))
            || self
                .column_selection
                .is_some_and(|range| contains(range, line, column))
    }
    pub fn status_message(&self) -> &str {
        &self.status_message
    }
    pub fn automation_debug(&self) -> JsonValue {
        json!({
            "focus": format!("{:?}", self.focus),
            "language": self.language(),
            "cursor": {"line": self.cursor().line + 1, "column": self.cursor().column + 1},
            "line_count": self.file_lines().len(),
            "status": self.status_message,
        })
    }
    pub fn automation_screenshot(&self) -> String {
        let (columns, rows) = self.automation_size.get();
        let backend = ratatui::backend::TestBackend::new(columns, rows);
        let Ok(mut terminal) = ratatui::Terminal::new(backend) else {
            return self.file_lines().join("\n");
        };
        if terminal.draw(|frame| crate::ui::draw(frame, self)).is_err() {
            return self.file_lines().join("\n");
        }
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(columns.max(1)))
            .map(|cells| {
                let line = cells.iter().map(|cell| cell.symbol()).collect::<String>();
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn search_query(&self) -> &str {
        &self.search_query
    }
    pub fn search_include(&self) -> &str {
        &self.search_include
    }
    pub fn search_exclude(&self) -> &str {
        &self.search_exclude
    }
    pub const fn search_field(&self) -> u8 {
        self.search_field
    }
    pub fn set_search_field(&mut self, field: u8, cursor: usize) {
        self.search_field = field.min(2);
        let text = match self.search_field {
            0 => &self.search_query,
            1 => &self.search_include,
            _ => &self.search_exclude,
        };
        self.search_cursor = cursor_from_display_column(text, cursor);
        self.sync_search_cursor();
        self.focus = Focus::Search;
    }
    pub fn set_replace_focus(&mut self, cursor: usize) {
        self.search_field = 0;
        self.replace_mode = true;
        self.replace_cursor = cursor_from_display_column(&self.replace_query, cursor);
        self.focus = Focus::Search;
    }
    pub const fn search_regex(&self) -> bool {
        self.search_regex
    }
    pub fn replace_query(&self) -> &str {
        &self.replace_query
    }
    pub const fn search_input_cursor(&self) -> usize {
        if self.search_field == 0 && self.replace_mode {
            self.replace_cursor
        } else {
            self.search_cursor
        }
    }
    pub const fn replace_mode(&self) -> bool {
        self.replace_mode
    }
    fn sync_search_cursor(&mut self) {
        if self.search_field == 0 && self.replace_mode {
            self.replace_cursor = self.replace_cursor.min(self.replace_query.chars().count());
        } else {
            let text = match self.search_field {
                0 => &self.search_query,
                1 => &self.search_include,
                _ => &self.search_exclude,
            };
            self.search_cursor = self.search_cursor.min(text.chars().count());
        }
    }
    fn edit_search_input(&mut self, key_name: &str, rune: Option<char>) -> bool {
        if self.search_field == 0 && self.replace_mode {
            edit_overlay_text(
                &mut self.replace_query,
                &mut self.replace_cursor,
                key_name,
                rune,
            )
            .unwrap_or(false)
        } else {
            let (text, cursor) = match self.search_field {
                0 => (&mut self.search_query, &mut self.search_cursor),
                1 => (&mut self.search_include, &mut self.search_cursor),
                _ => (&mut self.search_exclude, &mut self.search_cursor),
            };
            edit_overlay_text(text, cursor, key_name, rune).unwrap_or(false)
        }
    }
    pub fn search_results(&self) -> &[SearchResult] {
        &self.search_results
    }
    pub const fn search_index(&self) -> usize {
        self.search_index
    }
    pub fn active_search_match_column(&self, line: usize) -> Option<usize> {
        let selected = self.search_results.get(self.search_index)?;
        (selected.path.as_path() == self.buffer().path()? && selected.line == line + 1)
            .then(|| selected.column.saturating_sub(1))
    }
    pub fn changes(&self) -> &[GitChange] {
        &self.changes
    }
    pub fn change_label(&self, index: usize) -> String {
        let Some(change) = self.changes.get(index) else {
            return String::new();
        };
        let root = self.change_roots.get(index).unwrap_or(&self.search_root);
        if self.workspace_roots.len() > 1 {
            let name = root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("workspace");
            let duplicate_name = self
                .workspace_roots
                .iter()
                .filter(|candidate| {
                    candidate.file_name().and_then(|value| value.to_str()) == Some(name)
                })
                .count()
                > 1;
            let root_label = if duplicate_name {
                let parent_name = root
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|value| value.to_str())
                    .unwrap_or("workspace");
                let duplicate_parent = self
                    .workspace_roots
                    .iter()
                    .filter(|candidate| {
                        candidate.file_name().and_then(|value| value.to_str()) == Some(name)
                            && candidate
                                .parent()
                                .and_then(Path::file_name)
                                .and_then(|value| value.to_str())
                                == Some(parent_name)
                    })
                    .count()
                    > 1;
                if duplicate_parent {
                    root.to_string_lossy()
                        .trim_start_matches(std::path::MAIN_SEPARATOR)
                        .to_owned()
                } else {
                    format!("{parent_name}/{name}")
                }
            } else {
                name.to_owned()
            };
            format!("{root_label}/{}", change.path)
        } else {
            change.path.clone()
        }
    }
    pub fn changes_tree_rows(&self) -> Vec<ChangeTreeRow> {
        let tree_view = self.settings.git.file_view.eq_ignore_ascii_case("tree");
        let mut rows = Vec::new();
        for group in [ChangeGroup::Staged, ChangeGroup::Changes] {
            let mut indexes: Vec<_> = self
                .changes
                .iter()
                .enumerate()
                .filter_map(|(index, change)| match group {
                    ChangeGroup::Staged if change.index != ' ' && change.index != '?' => {
                        Some(index)
                    }
                    ChangeGroup::Changes if change.worktree != ' ' || change.index == '?' => {
                        Some(index)
                    }
                    _ => None,
                })
                .collect();
            indexes.sort_by_key(|index| self.change_label(*index));
            if indexes.is_empty() {
                continue;
            }
            let section_key = format!("section:{}", group.key());
            let section_expanded = !self.changes_tree_collapsed.contains(&section_key);
            rows.push(ChangeTreeRow {
                key: section_key,
                label: group.label().to_owned(),
                path: String::new(),
                group: Some(group),
                kind: ChangeTreeRowKind::Section,
                depth: 0,
                file_index: None,
                expanded: section_expanded,
                descendant_files: indexes.len(),
                status: String::new(),
                conflict: false,
            });
            if !section_expanded {
                continue;
            }
            if !tree_view {
                rows.extend(indexes.into_iter().map(|index| {
                    let change = &self.changes[index];
                    let path = self.change_label(index);
                    ChangeTreeRow {
                        key: format!("file:{}:{}", group.key(), path),
                        label: path.clone(),
                        path,
                        group: Some(group),
                        kind: ChangeTreeRowKind::File,
                        depth: 1,
                        file_index: Some(index),
                        expanded: false,
                        descendant_files: 1,
                        status: format!("{}{}", change.index, change.worktree),
                        conflict: change.is_conflict(),
                    }
                }));
                continue;
            }

            let mut root = ChangesTreeNode::default();
            for index in indexes {
                let label = self.change_label(index);
                let parts: Vec<_> = label.split('/').filter(|part| !part.is_empty()).collect();
                if parts.is_empty() {
                    continue;
                }
                let mut node = &mut root;
                for part in &parts[..parts.len().saturating_sub(1)] {
                    node = node.directories.entry((*part).to_owned()).or_default();
                }
                node.files.insert(parts[parts.len() - 1].to_owned(), index);
            }
            fn count_files(node: &ChangesTreeNode) -> usize {
                node.files.len() + node.directories.values().map(count_files).sum::<usize>()
            }
            fn flatten(
                node: &ChangesTreeNode,
                prefix: &str,
                group: ChangeGroup,
                depth: usize,
                collapsed: &BTreeSet<String>,
                changes: &[GitChange],
                rows: &mut Vec<ChangeTreeRow>,
            ) {
                let mut names: Vec<&str> = node
                    .directories
                    .keys()
                    .map(String::as_str)
                    .chain(node.files.keys().map(String::as_str))
                    .collect();
                names.sort_unstable();
                names.dedup();
                for name in names {
                    let path = if prefix.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{prefix}/{name}")
                    };
                    let key = format!("dir:{}:{path}", group.key());
                    if let Some(directory) = node.directories.get(name) {
                        let expanded = !collapsed.contains(&key);
                        rows.push(ChangeTreeRow {
                            key,
                            label: name.to_owned(),
                            path: path.clone(),
                            group: Some(group),
                            kind: ChangeTreeRowKind::Directory,
                            depth,
                            file_index: None,
                            expanded,
                            descendant_files: count_files(directory),
                            status: String::new(),
                            conflict: false,
                        });
                        if expanded {
                            flatten(directory, &path, group, depth + 1, collapsed, changes, rows);
                        }
                    } else if let Some(index) = node.files.get(name).copied() {
                        let change = &changes[index];
                        rows.push(ChangeTreeRow {
                            key: format!("file:{}:{path}", group.key()),
                            label: name.to_owned(),
                            path,
                            group: Some(group),
                            kind: ChangeTreeRowKind::File,
                            depth,
                            file_index: Some(index),
                            expanded: false,
                            descendant_files: 1,
                            status: format!("{}{}", change.index, change.worktree),
                            conflict: change.is_conflict(),
                        });
                    }
                }
            }
            flatten(
                &root,
                "",
                group,
                1,
                &self.changes_tree_collapsed,
                &self.changes,
                &mut rows,
            );
        }
        rows
    }
    pub fn changes_file_view(&self) -> &str {
        &self.settings.git.file_view
    }
    pub const fn changes_tree_scroll(&self) -> usize {
        self.changes_tree_scroll
    }
    pub fn changes_tree_row_selected(&self, row: &ChangeTreeRow) -> bool {
        if let Some(selection) = &self.changes_tree_selection {
            &row.key == selection
        } else {
            row.file_index == Some(self.change_index)
        }
    }
    pub fn changes_tree_hover(&mut self, visible_index: usize) {
        let index = self.changes_tree_scroll.saturating_add(visible_index);
        if let Some(row) = self.changes_tree_rows().get(index).cloned() {
            self.changes_tree_selection = Some(row.key);
            if let Some(change_index) = row.file_index {
                if self.change_index != change_index {
                    self.change_index = change_index;
                    self.refresh_change_diff();
                }
            }
        }
    }
    pub fn activate_changes_tree_row(&mut self, visible_index: usize) {
        let index = self.changes_tree_scroll.saturating_add(visible_index);
        let Some(row) = self.changes_tree_rows().get(index).cloned() else {
            return;
        };
        self.changes_tree_selection = Some(row.key.clone());
        if let Some(change_index) = row.file_index {
            self.change_index = change_index;
            self.refresh_change_diff();
        } else {
            if row.kind == ChangeTreeRowKind::Section {
                self.toggle_changes_tree_section(&row);
            } else {
                self.toggle_changes_tree_directory(&row, None);
            }
        }
    }
    fn change_root(&self, index: usize) -> &Path {
        self.change_roots
            .get(index)
            .map(PathBuf::as_path)
            .unwrap_or(&self.search_root)
    }
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }
    pub fn branches(&self) -> &[String] {
        &self.branches
    }
    pub const fn branch_index(&self) -> usize {
        self.branch_index
    }
    pub const fn branch_mode(&self) -> bool {
        self.branch_mode
    }
    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }
    pub const fn change_hunk_index(&self) -> usize {
        self.change_hunk_index
    }
    pub fn change_hunk_count(&self) -> usize {
        git_hunk_count(&self.change_diff)
    }
    #[allow(dead_code)]
    pub fn load_pull_request_review(&mut self, repository: &str, number: u64) -> AppResult<()> {
        if number == 0 || !repository.starts_with("https://github.com/") {
            return Err(crate::error::AppError::Git(
                "invalid GitHub pull request target".to_owned(),
            ));
        }
        let root = self.search_root.clone();
        let repository = repository.to_owned();
        let generation = self
            .pull_request_review_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        self.pull_request_comments_generation
            .fetch_add(1, Ordering::AcqRel);
        self.pull_request_comment_generation
            .fetch_add(1, Ordering::AcqRel);
        self.pull_request_comment_pending = false;
        self.pull_request_comments.clear();
        self.pull_request_comments_visible = false;
        let generation_state = Arc::clone(&self.pull_request_review_generation);
        let sender = self.pull_request_review_tx.clone();
        let worker_repository = repository.clone();
        let worker_root = root.clone();
        if std::thread::Builder::new()
            .name("mtide-pr-review".to_owned())
            .spawn(move || {
                let result = match git_fetch_pull_request(&worker_root, &worker_repository, number)
                {
                    Ok(reference) => {
                        let diff = git_diff_refs(&worker_root, "HEAD", &reference);
                        if diff.is_empty() {
                            Err("pull request has no reviewable diff".to_owned())
                        } else {
                            Ok((reference, diff))
                        }
                    }
                    Err(error) => Err(error.to_string()),
                };
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(PullRequestReviewCompletion {
                        generation,
                        root: worker_root,
                        repository: worker_repository,
                        number,
                        result,
                    });
                }
            })
            .is_err()
        {
            return Err(crate::error::AppError::Git(
                "unable to start pull request review".to_owned(),
            ));
        }
        self.status_message = format!("Fetching pull request #{number} review…");
        Ok(())
    }
    pub fn load_pull_request_comments(&mut self) -> AppResult<()> {
        let Some((repository, number)) = self.pull_request_target.clone() else {
            return Err(crate::error::AppError::Git(
                "no pull request review is active".to_owned(),
            ));
        };
        let generation = self
            .pull_request_comments_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        self.pull_request_comments_visible = true;
        let generation_state = Arc::clone(&self.pull_request_comments_generation);
        let sender = self.pull_request_comments_tx.clone();
        let worker_repository = repository.clone();
        if std::thread::Builder::new()
            .name("mtide-pr-comments".to_owned())
            .spawn(move || {
                let result = git_list_pr_comments(&worker_repository, number)
                    .map_err(|error| error.to_string());
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(PullRequestCommentsCompletion {
                        generation,
                        repository: worker_repository,
                        number,
                        result,
                    });
                }
            })
            .is_err()
        {
            self.pull_request_comments_visible = false;
            return Err(crate::error::AppError::Git(
                "unable to start pull request comments".to_owned(),
            ));
        }
        self.status_message = "Loading pull request comments…".to_owned();
        Ok(())
    }
    fn submit_pull_request_comment(&mut self) -> AppResult<()> {
        if self.pull_request_comment_pending {
            self.status_message = "Pull request comment is already submitting".to_owned();
            return Ok(());
        }
        let (Some((repository, number)), Some(reference)) = (
            self.pull_request_target.clone(),
            self.pull_request_head.clone(),
        ) else {
            return Err(crate::error::AppError::Git(
                "no pull request review is active".into(),
            ));
        };
        let path = self
            .change_diff
            .iter()
            .find_map(|line| line.text.strip_prefix("+++ b/").map(str::to_owned))
            .ok_or_else(|| crate::error::AppError::Git("review diff has no file path".into()))?;
        let line = self
            .change_diff
            .iter()
            .filter(|line| line.kind == '@')
            .nth(self.change_hunk_index)
            .and_then(|line| parse_new_hunk_line(&line.text))
            .ok_or_else(|| crate::error::AppError::Git("review hunk has no target line".into()))?;
        let root = self.search_root.clone();
        let body = self.commit_message.clone();
        let generation = self
            .pull_request_comment_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        let generation_state = Arc::clone(&self.pull_request_comment_generation);
        let sender = self.pull_request_comment_tx.clone();
        let worker_repository = repository.clone();
        let worker_reference = reference.clone();
        if std::thread::Builder::new()
            .name("mtide-pr-comment".to_owned())
            .spawn(move || {
                let result = match git_resolve_ref(&root, &worker_reference) {
                    Ok(commit_id) => crate::git::create_pull_request_comment(
                        &worker_repository,
                        number,
                        &commit_id,
                        &path,
                        line,
                        &body,
                    )
                    .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(PullRequestCommentCompletion {
                        generation,
                        repository: worker_repository,
                        number,
                        result,
                    });
                }
            })
            .is_err()
        {
            return Err(crate::error::AppError::Git(
                "unable to start pull request comment".to_owned(),
            ));
        }
        self.pull_request_comment_pending = true;
        self.commit_message.clear();
        self.commit_cursor = 0;
        self.pull_request_comment_mode = false;
        self.status_message = "Submitting pull request comment…".to_owned();
        Ok(())
    }
    fn jump_to_pull_request_comment(&mut self) -> AppResult<()> {
        let Some(comment) = self
            .pull_request_comments
            .get(self.output_scroll())
            .cloned()
        else {
            return Ok(());
        };
        let path = self.search_root.join(&comment.path);
        let mut buffer = Buffer::open(&path)?;
        if let Some(line) = comment.line {
            buffer.set_cursor(line.saturating_sub(1) as usize, 0);
        }
        self.open_buffer(buffer);
        self.focus = Focus::Editor;
        self.status_message = format!("Jumped to PR comment {}", comment.path);
        Ok(())
    }
    pub const fn commit_mode(&self) -> bool {
        self.commit_mode
    }
    pub fn commit_message(&self) -> &str {
        &self.commit_message
    }
    pub fn blame_lines(&self) -> &[BlameLine] {
        &self.blame_lines
    }
    pub fn history(&self) -> &[GitCommit] {
        &self.history
    }
    pub const fn commit_detail_active(&self) -> bool {
        self.commit_detail.is_some()
    }
    pub fn commit_detail_title(&self) -> Option<String> {
        let (commit, _, _) = self.commit_detail.as_ref()?;
        Some(format!(
            "Commit {} · v split/unified · c context {} · Esc history",
            &commit.hash[..commit.hash.len().min(8)],
            if self.diff_full_context {
                "full"
            } else {
                "changes"
            }
        ))
    }
    pub const fn history_has_more(&self) -> bool {
        self.history_has_more
    }
    pub const fn history_pending(&self) -> bool {
        self.history_pending
    }
    pub fn change_diff(&self) -> &[DiffLine] {
        &self.change_diff
    }
    pub fn rendered_change_diff(&self) -> Vec<DiffLine> {
        self.rendered_change_diff_with_map()
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }
    pub fn output_diff_lines(&self) -> Option<Vec<String>> {
        if self.commit_detail.is_some()
            || !self.blame_lines.is_empty()
            || !self.history.is_empty()
            || !self.plugin_registry_entries.is_empty()
            || (self.pull_request_comments_visible && !self.pull_request_comments.is_empty())
            || self.change_diff.is_empty()
        {
            return None;
        }
        Some(
            self.rendered_change_diff()
                .into_iter()
                .map(|line| {
                    if self.diff_split {
                        let (left, right) = match line.kind {
                            '-' => (line.text.as_str(), ""),
                            '+' => ("", line.text.as_str()),
                            _ => (line.text.as_str(), line.text.as_str()),
                        };
                        format!("{left:<42} │ {right}")
                    } else {
                        format!("{}{}", line.kind, line.text)
                    }
                })
                .collect(),
        )
    }
    pub fn output_reference_lines(&self) -> Option<Vec<String>> {
        if let Some((_, _, text)) = &self.commit_detail {
            let mut lines: Vec<String> = if self.diff_split {
                split_unified_diff_text(text)
                    .lines()
                    .map(ToOwned::to_owned)
                    .collect()
            } else {
                text.lines().map(ToOwned::to_owned).collect()
            };
            if self.commit_detail_pending {
                lines.push("… loading commit details".to_owned());
            }
            return Some(lines);
        }
        if !self.blame_lines.is_empty() {
            return Some(
                self.blame_lines
                    .iter()
                    .map(|line| {
                        format!(
                            "{} {} {}",
                            &line.commit[..line.commit.len().min(8)],
                            line.author,
                            line.line
                        )
                    })
                    .collect(),
            );
        }
        if !self.history.is_empty() {
            let mut lines = self
                .history
                .iter()
                .map(|commit| {
                    format!(
                        "{} {} {} {}",
                        &commit.hash[..commit.hash.len().min(8)],
                        commit.date,
                        commit.author,
                        commit.subject
                    )
                })
                .collect::<Vec<_>>();
            if self.history_has_more {
                lines.push("▶ Load older commits · press ]".to_owned());
            }
            return Some(lines);
        }
        if !self.plugin_registry_entries.is_empty() {
            return Some(
                self.plugin_registry_entries
                    .iter()
                    .map(|entry| format!("{} {} · {}", entry.name, entry.version, entry.url))
                    .collect(),
            );
        }
        if self.change_diff.is_empty() {
            let mut logs = self.plugin_logs();
            if logs.is_empty() {
                return Some(vec![
                    "[info] output panel ready".to_owned(),
                    "[info] select a Git change to inspect its diff".to_owned(),
                ]);
            }
            logs.reverse();
            return Some(logs);
        }
        if self.pull_request_comments_visible && !self.pull_request_comments.is_empty() {
            return Some(
                self.pull_request_comments
                    .iter()
                    .map(|comment| {
                        format!(
                            "{}:{} {}: {}",
                            comment.path,
                            comment.line.unwrap_or_default(),
                            comment
                                .user
                                .as_ref()
                                .map_or("unknown", |user| user.login.as_str()),
                            comment.body.replace('\n', " ")
                        )
                    })
                    .collect(),
            );
        }
        self.output_diff_lines()
    }
    pub fn output_hover_link(&self, line: usize) -> Option<&str> {
        self.output_hover_link
            .as_ref()
            .filter(|(hover_line, _)| *hover_line == line)
            .map(|(_, link)| link.as_str())
    }
    pub fn update_output_hover(&mut self, visible_row: usize, column: usize, control: bool) {
        self.output_hover_link = None;
        if !control {
            return;
        }
        let index = self.output_scroll.saturating_add(visible_row);
        let Some(line) = self
            .output_reference_lines()
            .and_then(|lines| lines.get(index).cloned())
        else {
            return;
        };
        let Some(column) = char_column_at_display_column(&line, column) else {
            return;
        };
        let Some(reference) = editor_link_at(&line, column) else {
            return;
        };
        let valid = if reference.starts_with("http://") || reference.starts_with("https://") {
            valid_http_url(&reference)
        } else {
            parse_editor_file_reference(&reference).is_some()
        };
        if valid {
            self.output_hover_link = Some((index, reference));
        }
    }
    pub fn open_output_link_at(&mut self, visible_row: usize, column: usize) -> bool {
        let index = self.output_scroll.saturating_add(visible_row);
        let Some(line) = self
            .output_reference_lines()
            .and_then(|lines| lines.get(index).cloned())
        else {
            return false;
        };
        let Some(column) = char_column_at_display_column(&line, column) else {
            return false;
        };
        let Some(reference) = editor_link_at(&line, column) else {
            return false;
        };
        if reference.starts_with("http://") || reference.starts_with("https://") {
            if !valid_http_url(&reference) {
                return false;
            }
            self.open_terminal_link(&reference);
        } else {
            let source = self
                .commit_detail
                .as_ref()
                .and_then(|(_, root, text)| commit_detail_source_path(root, text, index))
                .or_else(|| self.output_reference_source_path());
            let Some(target) = self.resolve_file_reference(&reference, source.as_deref()) else {
                return false;
            };
            if let Err(error) = self.open_external_file_target(target) {
                self.status_message = format!("Unable to open reference: {error}");
            }
        }
        self.output_hover_link = None;
        true
    }

    fn output_diff_source_path(&self) -> Option<PathBuf> {
        if self.pull_request_target.is_some() {
            return None;
        }
        let change = self.changes.get(self.change_index)?;
        Some(self.change_root(self.change_index).join(&change.path))
    }
    fn output_reference_source_path(&self) -> Option<PathBuf> {
        if self.commit_detail.is_some()
            || (self.pull_request_comments_visible && !self.pull_request_comments.is_empty())
            || self.pull_request_target.is_some()
            || !self.history.is_empty()
            || !self.plugin_registry_entries.is_empty()
        {
            return None;
        }
        if !self.change_diff.is_empty() {
            return self.output_diff_source_path();
        }
        self.buffer().path().map(Path::to_path_buf)
    }
    pub const fn diff_full_context(&self) -> bool {
        self.diff_full_context
    }
    pub const fn diff_word_wrap(&self) -> bool {
        self.settings.editor.diff_word_wrap
    }
    pub const fn diff_high_contrast(&self) -> bool {
        self.settings.editor.diff_high_contrast
    }
    pub const fn diff_view_active(&self) -> bool {
        self.commit_detail.is_some()
            || (!self.change_diff.is_empty()
                && (!self.pull_request_comments_visible || self.pull_request_comments.is_empty()))
    }
    fn rendered_change_diff_with_map(&self) -> Vec<(DiffLine, Option<usize>)> {
        if self.diff_full_context {
            return self
                .change_diff
                .iter()
                .cloned()
                .map(|line| (line, None))
                .collect();
        }
        let mut rendered = Vec::new();
        let mut index = 0;
        let mut gap = 0;
        while index < self.change_diff.len() {
            if self.change_diff[index].kind != ' ' {
                rendered.push((self.change_diff[index].clone(), None));
                index += 1;
                continue;
            }
            let start = index;
            while index < self.change_diff.len() && self.change_diff[index].kind == ' ' {
                index += 1;
            }
            let run = &self.change_diff[start..index];
            if run.len() <= 6 || self.diff_expanded_context.contains(&gap) {
                rendered.extend(run.iter().cloned().map(|line| (line, None)));
            } else {
                rendered.extend(run[..2].iter().cloned().map(|line| (line, None)));
                rendered.push((
                    DiffLine {
                        kind: '~',
                        text: format!(
                            "… {} context lines hidden · Enter/e expand",
                            run.len().saturating_sub(4)
                        ),
                    },
                    Some(gap),
                ));
                rendered.extend(
                    run[run.len() - 2..]
                        .iter()
                        .cloned()
                        .map(|line| (line, None)),
                );
            }
            gap += 1;
        }
        rendered
    }
    fn expand_diff_context_at_scroll(&mut self) -> bool {
        let rendered = self.rendered_change_diff_with_map();
        let Some((_, Some(gap))) = rendered.get(self.output_scroll) else {
            return false;
        };
        self.diff_expanded_context.insert(*gap);
        true
    }
    pub const fn diff_split(&self) -> bool {
        self.diff_split
    }
    pub fn pull_request_comments(&self) -> &[PullRequestComment] {
        &self.pull_request_comments
    }
    pub const fn pull_request_comments_visible(&self) -> bool {
        self.pull_request_comments_visible
    }
    pub const fn pull_request_review_active(&self) -> bool {
        self.pull_request_target.is_some()
    }
    pub const fn pull_request_comment_mode(&self) -> bool {
        self.pull_request_comment_mode
    }
    pub const fn output_scroll(&self) -> usize {
        self.output_scroll
    }
    fn output_max_scroll(&self) -> usize {
        if let Some((_, _, text)) = &self.commit_detail {
            return text.lines().count().saturating_sub(10);
        }
        self.rendered_change_diff()
            .len()
            .saturating_sub(1)
            .max(
                self.history
                    .len()
                    .saturating_add(usize::from(self.history_has_more))
                    .saturating_sub(1),
            )
            .max(self.blame_lines.len().saturating_sub(1))
            .max(self.plugin_registry_entries.len().saturating_sub(1))
            .max(if self.pull_request_comments_visible {
                self.pull_request_comments.len().saturating_sub(1)
            } else {
                0
            })
            .max(
                self.commit_detail
                    .as_ref()
                    .map_or(0, |(_, _, text)| text.lines().count().saturating_sub(1)),
            )
    }
    pub fn explorer_paths(&self) -> &[PathBuf] {
        &self.explorer_paths
    }
    pub fn explorer_label(&self, path: &Path) -> String {
        if self.explorer_roots.len() > 1 {
            if let Some(root) = self
                .explorer_roots
                .iter()
                .find(|root| path.starts_with(root))
            {
                let root_name = root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("workspace");
                let relative = path.strip_prefix(root).unwrap_or(path);
                if relative.as_os_str().is_empty() {
                    return root_name.to_owned();
                }
                return format!("{root_name}/{}", relative.display());
            }
        }
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?")
            .to_owned()
    }
    pub fn explorer_git_status(&self, path: &Path) -> Option<(char, char)> {
        self.workspace_roots.iter().find_map(|root| {
            let relative = path.strip_prefix(root).ok()?.to_string_lossy();
            let changes = if root == &self.search_root {
                Some(&self.changes)
            } else {
                self.git_changes_by_root.get(root)
            }?;
            changes
                .iter()
                .find(|change| change.path == relative)
                .map(|change| (change.index, change.worktree))
        })
    }
    pub fn explorer_depth(&self, path: &Path) -> usize {
        let roots = if self.explorer_roots.len() > 1 {
            &self.explorer_roots
        } else {
            return path
                .strip_prefix(self.explorer_roots.first().unwrap_or(&self.search_root))
                .map(|relative| relative.components().count())
                .unwrap_or(0);
        };
        roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok().map(|p| p.components().count()))
            .unwrap_or(0)
    }
    pub fn explorer_expanded(&self, path: &Path) -> bool {
        self.explorer_expanded.contains(path)
    }
    pub const fn explorer_index(&self) -> usize {
        self.explorer_index
    }
    pub fn explorer_mode(&self) -> Option<&str> {
        self.explorer_mode.as_ref().map(ExplorerMode::title)
    }
    pub fn explorer_input(&self) -> &str {
        &self.explorer_input
    }
    pub fn explorer_click(&mut self, index: usize) -> AppResult<()> {
        if index >= self.explorer_paths.len() {
            return Ok(());
        }
        self.focus = Focus::Explorer;
        self.explorer_index = index;
        self.handle_explorer_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }
    pub fn explorer_hover(&mut self, index: usize) {
        if index < self.explorer_paths.len() {
            self.explorer_index = index;
        }
    }
    pub fn select_sidebar_tab_by_index(&mut self, index: usize) {
        let tab = match index {
            0 => SidebarTab::Explorer,
            1 => SidebarTab::Search,
            2 => SidebarTab::Changes,
            3 => SidebarTab::Branches,
            4 => SidebarTab::Problems,
            _ => return,
        };
        self.select_sidebar(tab);
    }
    pub fn menu_click(&mut self, index: usize) {
        let label = ["File", "Edit", "Selection", "View", "Options", "Help"]
            .get(index)
            .copied()
            .unwrap_or("Menu");
        self.status_message = format!("{label} menu · use Ctrl-P for commands");
    }
    pub fn terminal_input(&self) -> &str {
        self.terminals[self.active_terminal].input()
    }
    pub fn terminal_title(&self) -> &str {
        self.terminals[self.active_terminal].title()
    }
    pub fn terminal_working_directory(&self) -> Option<&str> {
        self.terminals[self.active_terminal].working_directory()
    }
    pub fn terminal_screen_styled_lines(&self) -> Vec<Vec<(char, TerminalStyle)>> {
        let mut lines = self.terminals[self.active_terminal].screen_styled_lines();
        let Some((start, end)) = self.terminal_selection else {
            return lines;
        };
        let cell_lines = self.terminals[self.active_terminal].screen_cell_lines();
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let base = lines
            .len()
            .saturating_sub(self.terminal_viewport_rows.get().max(1));
        for (index, line) in lines.iter_mut().enumerate() {
            let visible_row = index.checked_sub(base);
            let Some(row) = visible_row else {
                continue;
            };
            let row = row as u16;
            if row < start.row || row > end.row {
                continue;
            }
            let from = if row == start.row { start.column } else { 0 };
            let to = if row == end.row {
                end.column
            } else {
                cell_lines.get(index).map_or(line.len(), Vec::len) as u16
            };
            let Some(cells) = cell_lines.get(index) else {
                continue;
            };
            let from = usize::from(from).min(cells.len());
            let to = usize::from(to).min(cells.len());
            let mut tuple_index = 0;
            for (cell_index, cell_text) in cells.iter().enumerate() {
                let tuple_len = cell_text.chars().count().max(1);
                if (from..to).contains(&cell_index) {
                    for (_, style) in line.iter_mut().skip(tuple_index).take(tuple_len) {
                        style.inverse = !style.inverse;
                    }
                }
                tuple_index = tuple_index.saturating_add(tuple_len);
            }
        }
        lines
    }
    pub fn begin_terminal_selection(&mut self, column: u16, row: u16) {
        let point = TerminalPoint { column, row };
        self.terminal_selection = Some((point, point));
        self.terminal_selection_dragging = true;
    }
    pub fn set_terminal_viewport_rows(&self, rows: usize) {
        self.terminal_viewport_rows.set(rows.max(1));
    }
    pub fn update_terminal_selection(&mut self, column: u16, row: u16) {
        if let Some((start, _)) = self.terminal_selection {
            self.terminal_selection = Some((start, TerminalPoint { column, row }));
        }
    }
    pub fn finish_terminal_selection(&mut self) {
        self.terminal_selection_dragging = false;
    }
    pub fn terminal_selection_text(&self) -> Option<String> {
        let (start, end) = self.terminal_selection?;
        let lines = self.terminals[self.active_terminal].screen_cell_lines();
        let base = lines
            .len()
            .saturating_sub(self.terminal_viewport_rows.get().max(1));
        let mut start = (usize::from(start.row) + base, usize::from(start.column));
        let mut end = (usize::from(end.row) + base, usize::from(end.column));
        if start > end {
            std::mem::swap(&mut start, &mut end);
        }
        let mut selected = Vec::new();
        let last_row = end.0.min(lines.len().saturating_sub(1));
        for (row, line) in lines.iter().enumerate().take(last_row + 1).skip(start.0) {
            let from = if row == start.0 {
                start.1.min(line.len())
            } else {
                0
            };
            let to = if row == end.0 {
                end.1.min(line.len())
            } else {
                line.len()
            };
            selected.push(line[from..to].concat().trim_end_matches(' ').to_owned());
        }
        Some(selected.join("\n"))
    }
    pub fn copy_terminal_selection(&mut self) {
        if let Some(text) = self
            .terminal_selection_text()
            .filter(|text| !text.is_empty())
        {
            self.clipboard = text;
            self.queue_osc52_clipboard();
            self.status_message = "Copied terminal selection".to_owned();
        }
    }
    pub fn scroll_terminal(&mut self, delta: isize) {
        if delta != 0 {
            self.terminal_selection = None;
            self.terminal_selection_dragging = false;
        }
        self.terminal_mut().scroll(delta);
    }

    pub fn terminal_mouse_tracking(&self) -> bool {
        self.terminals[self.active_terminal].mouse_tracking()
    }

    pub fn terminal_bracketed_paste(&self) -> bool {
        self.terminals[self.active_terminal].bracketed_paste()
    }

    pub fn handle_focus_event(&mut self, focused: bool) -> AppResult<()> {
        if !focused {
            self.editor_hover_link = None;
            self.plugin_view_hover_link = None;
            self.output_hover_link = None;
        }
        match self.focus {
            Focus::Terminal => {
                let _ = self.terminal_mut().handle_focus(focused)?;
            }
            Focus::Agent => {
                let _ = self.agent.handle_focus(focused)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn agent_mouse_tracking(&self) -> bool {
        self.agent.mouse_tracking()
    }

    pub fn send_agent_mouse(
        &mut self,
        column: u16,
        row: u16,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
    ) -> AppResult<bool> {
        self.agent.send_mouse_event(column, row, kind, modifiers)
    }

    pub fn terminal_alternate_screen(&self) -> bool {
        self.terminals[self.active_terminal].alternate_screen()
    }

    pub fn terminal_link_at(&self, column: u16, row: u16) -> Option<String> {
        self.terminals[self.active_terminal].link_at(column, row)
    }

    pub fn open_terminal_link(&mut self, url: &str) {
        if !valid_http_url(url) {
            self.status_message = "Blocked link: expected a valid HTTP(S) URL".to_owned();
            return;
        }
        let result = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(url).spawn()
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("rundll32.exe")
                .args(["url.dll,FileProtocolHandler", url])
                .spawn()
        } else {
            std::process::Command::new("xdg-open").arg(url).spawn()
        };
        self.status_message = match result {
            Ok(_) => format!("Opening {url}"),
            Err(error) => format!("Unable to open link: {error}"),
        };
    }

    pub fn editor_link_at(&self, position: Position) -> Option<String> {
        let reference = editor_link_at(self.file_lines().get(position.line)?, position.column)?;
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return Some(reference);
        }
        self.resolve_editor_file_reference(&reference)
            .map(|_| reference)
    }

    pub fn open_editor_link(&mut self, position: Position) -> bool {
        let Some(reference) = self.editor_link_at(position) else {
            return false;
        };
        if reference.starts_with("http://") || reference.starts_with("https://") {
            self.open_terminal_link(&reference);
        } else if let Some(target) = self.resolve_editor_file_reference(&reference) {
            match self.open_external_file_target(target) {
                Ok(()) => {}
                Err(error) => self.status_message = format!("Unable to open reference: {error}"),
            }
        }
        true
    }

    fn resolve_editor_file_reference(&self, reference: &str) -> Option<TargetSpec> {
        self.resolve_file_reference(reference, self.buffer().path())
    }

    fn resolve_file_reference(
        &self,
        reference: &str,
        source_path: Option<&Path>,
    ) -> Option<TargetSpec> {
        let mut target = parse_editor_file_reference(reference)?;
        if !target.path.is_absolute() {
            let source_directory = source_path
                .and_then(Path::parent)
                .and_then(|path| path.canonicalize().ok());
            let base = source_directory
                .or_else(|| self.workspace_roots.first().cloned())
                .or_else(|| std::env::current_dir().ok())?;
            target.path = base.join(target.path);
        }
        target.path = target.path.canonicalize().ok()?;
        target.path.is_file().then_some(target)
    }

    fn open_external_file_target(&mut self, target: TargetSpec) -> AppResult<()> {
        let mut buffer = Buffer::open(&target.path)?;
        buffer.set_cursor(target.line, target.column);
        self.open_buffer(buffer);
        self.focus = Focus::Editor;
        self.sync_lsp_for_buffer();
        self.emit_plugin_event("document.open");
        self.status_message = format!("Opened {}", target.path.display());
        Ok(())
    }

    pub fn update_editor_hover(&mut self, position: Option<Position>, control: bool) {
        self.editor_hover_link = if control {
            position.and_then(|position| {
                let line = self.file_lines().get(position.line)?;
                let reference = editor_link_at(line, position.column)?;
                let valid = if reference.starts_with("http://") || reference.starts_with("https://")
                {
                    valid_http_url(&reference)
                } else {
                    parse_editor_file_reference(&reference).is_some()
                };
                valid.then_some((position.line, reference))
            })
        } else {
            None
        };
    }

    pub fn editor_hover_link(&self, line: usize) -> Option<&str> {
        self.editor_hover_link
            .as_ref()
            .filter(|(hover_line, _)| *hover_line == line)
            .map(|(_, url)| url.as_str())
    }

    fn reload_active_buffer(&mut self) -> AppResult<()> {
        if self.buffer_mut().reload_from_disk()? {
            self.sync_lsp_for_buffer();
            self.refresh_search_results();
            self.emit_plugin_event("document.reload");
            self.status_message = "Reloaded current file from disk".to_owned();
        } else {
            self.status_message = "Welcome buffer has no file on disk".to_owned();
        }
        Ok(())
    }

    pub fn send_terminal_mouse(
        &mut self,
        column: u16,
        row: u16,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
    ) -> AppResult<bool> {
        self.terminal_mut()
            .send_mouse_event(column, row, kind, modifiers)
    }
    pub const fn active_terminal(&self) -> usize {
        self.active_terminal
    }
    pub const fn terminal_count(&self) -> usize {
        self.terminals.len()
    }
    pub fn agent_input(&self) -> &str {
        self.agent.input()
    }
    pub fn agent_screen_lines(&self) -> Vec<String> {
        self.agent.screen_lines()
    }
    pub fn agent_name(&self) -> &str {
        self.settings
            .agent
            .providers
            .get(&self.agent_provider)
            .map(|provider| {
                if provider.name.is_empty() {
                    self.agent_provider.as_str()
                } else {
                    provider.name.as_str()
                }
            })
            .unwrap_or(&self.settings.agent.name)
    }
    pub fn agent_configured(&self) -> bool {
        self.settings.agent.enabled
            && self
                .settings
                .agent
                .providers
                .get(&self.agent_provider)
                .map_or(!self.settings.agent.command.is_empty(), |provider| {
                    !provider.command.is_empty()
                })
    }
    #[allow(dead_code)]
    pub fn agent_tool_allowed(&self, tool: &str) -> bool {
        self.agent_allowed_tools()
            .iter()
            .any(|allowed| allowed == tool)
    }

    fn agent_allowed_tools(&self) -> Vec<String> {
        self.settings
            .agent
            .providers
            .get(&self.agent_provider)
            .map(|provider| {
                if provider.tools.is_empty() {
                    self.settings.agent.tools.clone()
                } else {
                    provider.tools.clone()
                }
            })
            .unwrap_or_else(|| self.settings.agent.tools.clone())
    }

    #[allow(dead_code)]
    pub fn agent_tool_call(&self, tool: &str, args: &JsonValue) -> AppResult<JsonValue> {
        let protected_paths = self
            .buffers
            .iter()
            .filter(|buffer| buffer.dirty())
            .filter_map(|buffer| buffer.path())
            .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
            .collect::<Vec<_>>();
        execute_agent_tool_call(
            tool,
            args,
            &self.agent_allowed_tools(),
            &self.workspace_roots,
            &protected_paths,
        )
    }
    pub fn agent_install_confirm(&self) -> bool {
        self.agent_install_confirm
    }
    pub fn agent_install_command(&self) -> Option<&str> {
        self.settings
            .agent
            .providers
            .get(&self.agent_provider)
            .and_then(|provider| provider.install.as_deref())
            .or(self.settings.agent.install.as_deref())
    }
    pub fn agent_executable_available(&self) -> bool {
        let Some(executable) = self
            .settings
            .agent
            .providers
            .get(&self.agent_provider)
            .map(|provider| provider.command.first())
            .unwrap_or_else(|| self.settings.agent.command.first())
        else {
            return false;
        };
        executable_available(executable)
    }
    pub const fn should_quit(&self) -> bool {
        self.quit
    }
    pub fn quit(&mut self) {
        if self.settings.save_all_on_quit {
            let editor = self.settings.editor.clone();
            let mut saved = 0;
            let mut saved_lsp_documents = Vec::new();
            for buffer in &mut self.buffers {
                if !buffer.dirty() {
                    continue;
                }
                if buffer.has_unresolved_conflicts() {
                    self.status_message =
                        "Save all blocked: resolve merge conflicts first".to_owned();
                    return;
                }
                if buffer.has_external_change() {
                    self.status_message =
                        "Save all blocked: reload or Save As before overwriting disk changes"
                            .to_owned();
                    return;
                }
                if let Err(error) = buffer
                    .save_with_options(editor.trim_trailing_whitespace, editor.insert_final_newline)
                {
                    self.status_message = format!("Save all on quit failed: {error}");
                    return;
                }
                if let Some(path) = buffer.path().map(Path::to_path_buf) {
                    let language = buffer
                        .language_override()
                        .map(str::to_owned)
                        .or_else(|| editor.language.clone())
                        .or_else(|| {
                            path.extension()
                                .and_then(|extension| extension.to_str())
                                .map(str::to_owned)
                        })
                        .unwrap_or_default();
                    saved_lsp_documents.push((language, path));
                }
                saved += 1;
            }
            for (language, path) in saved_lsp_documents {
                self.notify_lsp_saved(&language, &path);
            }
            if saved > 0 {
                self.status_message = format!("Saved {saved} document(s)");
            }
        }
        let open_lsp_documents = self
            .buffers
            .iter()
            .filter_map(|buffer| {
                let path = buffer.path()?.to_path_buf();
                let language = buffer
                    .language_override()
                    .map(str::to_owned)
                    .or_else(|| self.settings.editor.language.clone())
                    .or_else(|| {
                        path.extension()
                            .and_then(|extension| extension.to_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                Some((language, path))
            })
            .collect::<Vec<_>>();
        for (language, path) in open_lsp_documents {
            self.notify_lsp_closed(&language, &path);
        }
        if let Some(path) = self.agent_session_path.as_deref() {
            if let Err(error) = save_agent_session(
                path,
                &AgentSession {
                    version: AGENT_SESSION_VERSION,
                    provider: self.agent_provider.clone(),
                    input: self.agent.input().to_owned(),
                    screen: self.agent.screen_lines(),
                },
            ) {
                self.status_message = format!("Agent session save failed: {error}");
            }
        }
        self.save_language_overrides();
        self.quit = true;
    }
    fn save_language_overrides(&mut self) {
        let Some(path) = self.language_overrides_path.clone() else {
            return;
        };
        if let Err(error) = save_language_overrides(&path, &self.language_overrides) {
            self.status_message = format!("Language override save failed: {error}");
        }
    }
    pub fn set_status_message(&mut self, message: String) {
        self.status_message = message;
    }
    pub fn take_osc52_clipboard(&mut self) -> Option<String> {
        self.osc52_clipboard.take()
    }
    fn queue_osc52_clipboard(&mut self) {
        self.osc52_clipboard = Some(self.clipboard.clone());
    }
    fn request_search_refresh(&mut self) {
        let generation = self.search_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation_state = Arc::clone(&self.search_generation);
        let sender = self.search_tx.clone();
        let roots = self.workspace_roots.clone();
        let query = self.search_query.clone();
        let include = Self::parse_search_globs(&self.search_include);
        let exclude = Self::parse_search_globs(&self.search_exclude);
        let use_regex = self.search_regex;
        self.search_pending = true;
        if std::thread::Builder::new()
            .name("mtide-search".to_owned())
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(75));
                if generation_state.load(Ordering::Acquire) != generation {
                    return;
                }
                let results = search_workspace_roots_with_options_cancelled(
                    &roots,
                    &query,
                    &include,
                    &exclude,
                    use_regex,
                    Some((&generation_state, generation)),
                );
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(SearchCompletion {
                        generation,
                        results,
                    });
                }
            })
            .is_err()
        {
            self.search_pending = false;
            self.refresh_search_results();
        }
    }
    fn poll_search_results(&mut self) {
        while let Ok(completion) = self.search_rx.try_recv() {
            if completion.generation != self.search_generation.load(Ordering::Acquire) {
                continue;
            }
            self.search_pending = false;
            self.search_results = completion.results;
            self.merge_open_buffer_search_results();
            self.search_index = self
                .search_index
                .min(self.search_results.len().saturating_sub(1));
            if !self.search_query.is_empty() {
                self.status_message = format!("{} matches", self.search_results.len());
            }
        }
    }

    fn poll_markdown_image(&mut self) {
        while let Ok(completion) = self.markdown_image_rx.try_recv() {
            self.markdown_image_pending = false;
            let active_path = self.buffer().path().map(Path::to_path_buf);
            if active_path.as_ref() != Some(&completion.document) || self.focus != Focus::Editor {
                continue;
            }
            let Some(bytes) = completion.bytes else {
                self.status_message = "No PNG image found in clipboard".to_owned();
                continue;
            };
            if let Err(error) = self.insert_markdown_image_bytes(&bytes) {
                self.status_message = format!("Markdown image paste failed: {error}");
            }
        }
    }

    fn autosave_dirty_buffers(&mut self) {
        let editor = self.settings.editor.clone();
        let mut saved = 0usize;
        let mut saved_lsp_documents = Vec::new();
        for buffer in &mut self.buffers {
            if !buffer.dirty() || buffer.has_unresolved_conflicts() || buffer.has_external_change()
            {
                continue;
            }
            if buffer
                .save_with_options(editor.trim_trailing_whitespace, editor.insert_final_newline)
                .is_err()
            {
                continue;
            }
            if let Some(path) = buffer.path().map(Path::to_path_buf) {
                let language = buffer
                    .language_override()
                    .map(str::to_owned)
                    .or_else(|| editor.language.clone())
                    .or_else(|| {
                        path.extension()
                            .and_then(|extension| extension.to_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                saved_lsp_documents.push((language, path));
            }
            saved += 1;
        }
        for (language, path) in saved_lsp_documents {
            self.notify_lsp_saved(&language, &path);
        }
        if saved > 0 {
            self.refresh_git_state();
            self.status_message = format!("Auto-saved {saved} document(s)");
        }
    }

    pub fn poll_background(&mut self) {
        if self.settings.editor.auto_save
            && self.last_autosave.elapsed()
                >= Duration::from_millis(self.settings.editor.auto_save_delay_ms)
        {
            self.last_autosave = Instant::now();
            self.autosave_dirty_buffers();
        }
        self.poll_search_results();
        self.poll_markdown_image();
        while let Ok(completion) = self.git_blame_rx.try_recv() {
            if completion.generation != self.git_blame_generation.load(Ordering::Acquire) {
                continue;
            }
            let active_path = self.buffer().path().map(Path::to_path_buf);
            let active_root = self.active_git_root();
            let active_relative = active_path
                .as_deref()
                .and_then(|path| path.strip_prefix(&active_root).ok())
                .map(|path| path.to_string_lossy().into_owned());
            if active_root == completion.root
                && active_relative.as_deref() == Some(completion.relative.as_str())
            {
                self.blame_lines = completion.lines;
                self.bottom_tab = BottomTab::Output;
                self.status_message = if self.blame_lines.is_empty() {
                    "No blame data available".to_owned()
                } else {
                    format!("Loaded blame for {} lines", self.blame_lines.len())
                };
            }
        }
        while let Ok(completion) = self.git_history_rx.try_recv() {
            if completion.generation != self.git_history_generation.load(Ordering::Acquire) {
                continue;
            }
            if self.active_git_root() == completion.root {
                self.history_root = Some(completion.root.clone());
                if completion.append {
                    self.history.extend(completion.commits);
                } else {
                    self.history = completion.commits;
                    self.history_snapshot_head = completion.snapshot_head;
                }
                self.history_offset = self.history.len();
                self.history_has_more = completion.has_more;
                self.history_pending = false;
                self.bottom_tab = BottomTab::Output;
                self.status_message = if self.history_has_more {
                    format!("Loaded {} commits · ] for more", self.history.len())
                } else {
                    format!("Loaded {} commits", self.history.len())
                };
            }
        }
        while let Ok(completion) = self.commit_detail_rx.try_recv() {
            if completion.generation != self.commit_detail_generation.load(Ordering::Acquire) {
                continue;
            }
            let Some((commit, root, text)) = self.commit_detail.as_mut() else {
                continue;
            };
            if *root != completion.root || commit.hash != completion.hash {
                continue;
            }
            self.commit_detail_pending = false;
            match completion.result {
                Ok(output) => {
                    *text = output;
                    self.status_message = format!(
                        "Commit {} · v split/unified · c context · Esc history · Ctrl-click opens references",
                        &completion.hash[..completion.hash.len().min(8)]
                    );
                }
                Err(error) => {
                    *text = format!("Unable to load commit details: {error}");
                    self.status_message = "Git commit details failed".to_owned();
                }
            }
        }
        while let Ok(completion) = self.git_state_rx.try_recv() {
            if completion.generation != self.git_state_generation.load(Ordering::Acquire) {
                continue;
            }
            if completion.active_root != self.active_git_root() {
                // A tab/workspace-root switch can happen while the worker is
                // collecting Git state. Keep the UI recoverable even when no
                // later action explicitly requests another refresh.
                self.git_state_pending = false;
                self.refresh_git_state();
                continue;
            }
            self.changes = completion.changes;
            self.git_state_pending = false;
            self.change_roots = completion.change_roots;
            self.git_changes_by_root = completion.changes_by_root;
            self.branch = completion.branch;
            self.branches = completion.branches;
            self.branch_index = self
                .branch
                .as_ref()
                .and_then(|current| self.branches.iter().position(|branch| branch == current))
                .unwrap_or(0);
            self.change_index = self.change_index.min(self.changes.len().saturating_sub(1));
            let tree_rows = self.changes_tree_rows();
            if self
                .changes_tree_selection
                .as_ref()
                .is_some_and(|key| !tree_rows.iter().any(|row| &row.key == key))
            {
                self.changes_tree_selection = tree_rows
                    .iter()
                    .find(|row| row.file_index == Some(self.change_index))
                    .map(|row| row.key.clone());
            }
            self.changes_tree_scroll = self
                .changes_tree_scroll
                .min(tree_rows.len().saturating_sub(12));
            self.change_diff = completion.change_diff;
            self.diff_expanded_context.clear();
            self.change_hunk_index = self
                .change_hunk_index
                .min(git_hunk_count(&self.change_diff).saturating_sub(1));
            self.output_scroll = 0;
        }
        while let Ok(completion) = self.pull_request_review_rx.try_recv() {
            if completion.generation != self.pull_request_review_generation.load(Ordering::Acquire)
                || completion.root != self.search_root
            {
                continue;
            }
            match completion.result {
                Ok((reference, diff)) => {
                    self.change_diff = diff;
                    self.change_hunk_index = 0;
                    self.diff_split = false;
                    self.pull_request_target = Some((completion.repository, completion.number));
                    self.pull_request_head = Some(reference);
                    self.pull_request_comments.clear();
                    self.pull_request_comments_visible = false;
                    self.pull_request_comment_mode = false;
                    self.bottom_tab = BottomTab::Output;
                    self.status_message =
                        format!("Loaded pull request #{} review diff", completion.number);
                }
                Err(error) => {
                    self.status_message = format!("Pull request review failed: {error}");
                }
            }
        }
        while let Ok(completion) = self.pull_request_comments_rx.try_recv() {
            if completion.generation
                != self
                    .pull_request_comments_generation
                    .load(Ordering::Acquire)
                || self.pull_request_target.as_ref()
                    != Some(&(completion.repository.clone(), completion.number))
            {
                continue;
            }
            match completion.result {
                Ok(comments) => {
                    self.pull_request_comments = comments;
                    self.pull_request_comments_visible = true;
                    self.output_scroll = 0;
                    self.status_message = format!(
                        "Loaded {} pull request review comments",
                        self.pull_request_comments.len()
                    );
                }
                Err(error) => {
                    self.pull_request_comments_visible = false;
                    self.status_message = format!("PR comments failed: {error}");
                }
            }
        }
        while let Ok(completion) = self.pull_request_comment_rx.try_recv() {
            if completion.generation != self.pull_request_comment_generation.load(Ordering::Acquire)
                || self.pull_request_target.as_ref()
                    != Some(&(completion.repository.clone(), completion.number))
            {
                continue;
            }
            self.pull_request_comment_pending = false;
            match completion.result {
                Ok(comment) => {
                    self.pull_request_comments.push(comment);
                    self.status_message = "Pull request comment submitted".to_owned();
                }
                Err(error) => {
                    self.status_message = format!("Pull request comment failed: {error}");
                }
            }
        }
        self.poll_ai_completions();
        if self.completion_due.is_some_and(|due| Instant::now() >= due) {
            self.completion_due = None;
            let language = self.language().to_owned();
            if self.settings.lsp.enabled
                && !language.is_empty()
                && self.lsp.configured(&language)
                && self.lsp_popup.is_none()
            {
                if let Err(error) = self.request_lsp("textDocument/completion") {
                    self.status_message = format!("Completion unavailable: {error}");
                }
            } else if self.settings.ai.enabled && self.settings.ai.auto_suggest {
                self.request_ai_completion();
            }
        }
        while let Ok(completion) = self.git_remote_rx.try_recv() {
            self.git_remote_pending = None;
            match completion.result {
                Ok(()) => {
                    if self.active_git_root() == completion.root {
                        self.refresh_git_state();
                    }
                    self.status_message = format!(
                        "Git {} completed · {}",
                        completion.operation,
                        completion.root.display()
                    );
                }
                Err(error) => {
                    self.handle_git_remote_failure(&completion.operation, &completion.root, &error)
                }
            }
        }
        let mut active_reloaded = false;
        for index in 0..self.buffers.len() {
            let path = self.buffers[index].path().map(Path::to_path_buf);
            let result = self.buffers[index].poll_external_change();
            match result {
                Ok(Some(ExternalChange::Reloaded)) => {
                    if index == self.active_tab {
                        active_reloaded = true;
                    }
                    if let Some(path) = path {
                        self.status_message =
                            format!("Reloaded external changes · {}", path.display());
                    }
                }
                Ok(Some(ExternalChange::Dirty)) => {
                    if let Some(path) = path {
                        self.status_message = format!(
                            "Changed on disk; save or reload manually · {}",
                            path.display()
                        );
                    }
                }
                Ok(Some(ExternalChange::Deleted)) => {
                    if let Some(path) = path {
                        self.status_message = format!(
                            "File deleted on disk; save to recreate or close tab · {}",
                            path.display()
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => self.status_message = format!("External reload failed: {error}"),
            }
        }
        if active_reloaded {
            self.sync_lsp_for_buffer();
            self.refresh_search_results();
            self.emit_plugin_event("document.reload");
        }
        for terminal in &mut self.terminals {
            terminal.poll();
            if let Some(text) = terminal.take_osc52_clipboard() {
                self.clipboard = text;
                self.status_message = "Terminal updated clipboard".to_owned();
            }
            if terminal.take_bell() {
                self.status_message = "Terminal bell".to_owned();
            }
        }
        self.agent.poll();
        self.poll_agent_protocol();
        if self.agent_install_pending && self.agent_executable_available() {
            self.agent_install_pending = false;
            self.restart_agent_session();
            self.status_message =
                format!("Agent {} installed · session restarted", self.agent_name());
        }
        for (language, message) in self.lsp.poll() {
            if message.get("id").is_some()
                && message.get("method").and_then(JsonValue::as_str) != Some("workspace/applyEdit")
            {
                self.handle_lsp_server_request(&language, &message);
            } else if message.get("method").and_then(JsonValue::as_str)
                == Some("workspace/applyEdit")
            {
                self.handle_lsp_apply_edit_request(&language, &message);
            } else if matches!(
                message.get("method").and_then(JsonValue::as_str),
                Some("window/showMessage") | Some("window/logMessage")
            ) {
                if let Some(text) = message
                    .get("params")
                    .and_then(|params| params.get("message"))
                    .and_then(JsonValue::as_str)
                {
                    let prefix = if message.get("method").and_then(JsonValue::as_str)
                        == Some("window/showMessage")
                    {
                        "LSP"
                    } else {
                        "LSP log"
                    };
                    self.status_message = format!("{prefix}: {text}");
                }
            } else {
                self.lsp.apply_notification(&message);
            }
        }
        self.poll_lsp_responses();
        for runtime in &self.plugin_runtimes {
            if let Err(error) = runtime.poll_async() {
                self.status_message = format!("Plugin async task failed: {error}");
            }
        }
        self.apply_plugin_panel_requests();
        self.apply_plugin_automation_requests();
        self.sync_plugin_clipboard();
        self.refresh_plugin_custom_tab();
        self.emit_plugin_event("app.tick");
    }

    fn handle_lsp_server_request(&mut self, language: &str, message: &JsonValue) {
        let Some(id) = message.get("id") else {
            return;
        };
        let method = message
            .get("method")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let result = match method {
            "workspace/configuration" => message
                .get("params")
                .and_then(|params| params.get("items"))
                .and_then(JsonValue::as_array)
                .map(|items| vec![JsonValue::Null; items.len()])
                .map(JsonValue::Array)
                .unwrap_or_else(|| JsonValue::Array(Vec::new())),
            "workspace/workspaceFolders" => JsonValue::Array(
                self.workspace_roots
                    .iter()
                    .map(|root| {
                        json!({
                            "uri": path_to_uri(root),
                            "name": root
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("workspace")
                        })
                    })
                    .collect(),
            ),
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest" => JsonValue::Null,
            _ => JsonValue::Null,
        };
        if let Err(error) = self.lsp.respond(language, id, result) {
            self.status_message = format!("LSP server request failed: {error}");
        }
    }

    fn poll_lsp_responses(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .lsp_pending
            .iter()
            .filter(|(_, request)| request.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for (language, id) in expired {
            let format_save_request =
                self.lsp_pending
                    .get(&(language.clone(), id))
                    .is_some_and(|request| {
                        matches!(
                            &request.kind,
                            PendingLspKind::Edits { method, .. }
                                if method == "textDocument/formatting"
                        )
                    });
            self.lsp_pending.remove(&(language.clone(), id));
            let _ = self.lsp.cancel_request(&language, id);
            self.status_message = "LSP request timed out".to_owned();
            if format_save_request && self.format_on_save_pending {
                self.format_on_save_pending = false;
                let editor = self.settings.editor.clone();
                if let Err(error) = self.save_active_buffer_now(&editor) {
                    self.status_message = format!("Save after format timeout failed: {error}");
                }
            }
        }
        let responses = self.lsp.poll_responses();
        for (language, id, response) in responses {
            let Some(request) = self.lsp_pending.remove(&(language, id)) else {
                continue;
            };
            let current_path = self.buffer().path();
            if request.generation != self.lsp_generation
                || current_path != Some(request.path.as_path())
            {
                continue;
            }
            let format_save_request = matches!(
                &request.kind,
                PendingLspKind::Edits { method, .. }
                    if method == "textDocument/formatting"
            );
            match response {
                Ok(result) => {
                    let result = match request.kind {
                        PendingLspKind::Query(method) => {
                            self.handle_lsp_result(&method, &request.path, result)
                        }
                        PendingLspKind::Rename { uri } => {
                            self.handle_lsp_rename_result(&uri, result)
                        }
                        PendingLspKind::Edits {
                            method,
                            request_method,
                            uri,
                        } => self.handle_lsp_edits_result(&method, &request_method, &uri, result),
                        PendingLspKind::CompletionResolve {
                            item,
                            commit_character,
                        } => self.handle_completion_resolve_result(item, commit_character, result),
                        PendingLspKind::ExecuteCommand { uri } => {
                            self.handle_lsp_execute_command_result(&uri, result)
                        }
                    };
                    if let Err(error) = result {
                        self.status_message = format!("LSP response failed: {error}");
                    }
                }
                Err(error) => self.status_message = format!("LSP request failed: {error}"),
            }
            if format_save_request && self.format_on_save_pending {
                self.format_on_save_pending = false;
                let editor = self.settings.editor.clone();
                if let Err(error) = self.save_active_buffer_now(&editor) {
                    self.status_message = format!("Save after formatting failed: {error}");
                }
            }
        }
    }

    fn handle_lsp_apply_edit_request(&mut self, language: &str, message: &JsonValue) {
        let Some(id) = message.get("id") else {
            return;
        };
        let current_uri = self.buffer().path().map(path_to_uri).unwrap_or_default();
        let result = message
            .get("params")
            .and_then(|params| params.get("edit"))
            .ok_or_else(|| {
                crate::error::AppError::Lsp("workspace/applyEdit is missing edit".to_owned())
            })
            .and_then(|edit| self.apply_workspace_edit(&current_uri, edit))
            .map(|changed| json!({"applied": true, "changeCount": changed}))
            .unwrap_or_else(|error| json!({"applied": false, "failureReason": error.to_string()}));
        if let Err(error) = self.lsp.respond(language, id, result) {
            self.status_message = format!("LSP applyEdit response failed: {error}");
        }
    }

    fn poll_agent_tool_results(&mut self) {
        let generation = self.agent_tool_generation.load(Ordering::Acquire);
        while let Ok(completion) = self.agent_tool_rx.try_recv() {
            if completion.generation != generation {
                continue;
            }
            let mut encoded = match serde_json::to_vec(&completion.response) {
                Ok(encoded) => encoded,
                Err(error) => {
                    self.status_message = format!("Agent tool response encoding failed: {error}");
                    continue;
                }
            };
            encoded.push(b'\n');
            if let Err(error) = self.agent.write_bytes(&encoded) {
                self.status_message = format!("Agent tool response failed: {error}");
            }
        }
    }

    fn poll_agent_protocol(&mut self) {
        self.poll_agent_tool_results();
        self.agent_protocol_buffer
            .extend_from_slice(&self.agent.take_recent_output());
        if self.agent_protocol_buffer.len() > 1024 * 1024 {
            let drain = self.agent_protocol_buffer.len() - 1024 * 1024;
            self.agent_protocol_buffer.drain(..drain);
        }
        while let Some(newline) = self
            .agent_protocol_buffer
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let line: Vec<u8> = self.agent_protocol_buffer.drain(..=newline).collect();
            let Ok(message) = serde_json::from_slice::<JsonValue>(&line) else {
                continue;
            };
            if message.get("type").and_then(JsonValue::as_str) != Some("tool_call") {
                continue;
            }
            let id = message.get("id").cloned().unwrap_or(JsonValue::Null);
            let Some(tool) = message.get("tool").and_then(JsonValue::as_str) else {
                if !id.is_null() {
                    let generation = self.agent_tool_generation.load(Ordering::Acquire);
                    let _ = self.agent_tool_tx.send(AgentToolCompletion {
                        generation,
                        response: agent_tool_error_response(
                            id,
                            "agent tool_call requires a string tool field",
                        ),
                    });
                }
                continue;
            };
            let args = message.get("args").cloned().unwrap_or_else(|| json!({}));
            let inflight = Arc::clone(&self.agent_tool_inflight);
            if inflight
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < AGENT_TOOL_MAX_CONCURRENT).then_some(count + 1)
                })
                .is_err()
            {
                let response = json!({
                    "type": "tool_result",
                    "id": id,
                    "ok": false,
                    "error": "agent tool concurrency limit reached",
                });
                let generation = self.agent_tool_generation.load(Ordering::Acquire);
                let _ = self.agent_tool_tx.send(AgentToolCompletion {
                    generation,
                    response,
                });
                continue;
            }
            let allowed_tools = self.agent_allowed_tools();
            let workspace_roots = self.workspace_roots.clone();
            let protected_paths = self
                .buffers
                .iter()
                .filter(|buffer| buffer.dirty())
                .filter_map(|buffer| buffer.path())
                .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
                .collect::<Vec<_>>();
            let tx = self.agent_tool_tx.clone();
            let generation = self.agent_tool_generation.load(Ordering::Acquire);
            let generation_state = Arc::clone(&self.agent_tool_generation);
            let tool = tool.to_owned();
            std::thread::spawn(move || {
                if generation_state.load(Ordering::Acquire) != generation {
                    inflight.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
                let response = match execute_agent_tool_call(
                    &tool,
                    &args,
                    &allowed_tools,
                    &workspace_roots,
                    &protected_paths,
                ) {
                    Ok(result) => {
                        json!({"type":"tool_result", "id":id, "ok":true, "result":result})
                    }
                    Err(error) => {
                        json!({"type":"tool_result", "id":id, "ok":false, "error":error.to_string()})
                    }
                };
                let _ = tx.send(AgentToolCompletion {
                    generation,
                    response,
                });
                inflight.fetch_sub(1, Ordering::AcqRel);
            });
        }
    }
    fn apply_plugin_panel_requests(&mut self) {
        let requests = self
            .plugin_runtimes
            .iter()
            .enumerate()
            .flat_map(|(runtime_index, runtime)| {
                runtime
                    .drain_panel_requests()
                    .into_iter()
                    .map(move |request| (runtime_index, request))
            })
            .collect::<Vec<_>>();
        for (runtime_index, request) in requests {
            match request {
                PluginPanelRequest::ReloadTheme => self.reload_theme_from_settings(),
                PluginPanelRequest::FocusTerminal => {
                    self.bottom_visible = true;
                    self.bottom_tab = BottomTab::Terminal;
                    self.focus = Focus::Terminal;
                }
                PluginPanelRequest::Show { location, focus } => match location {
                    PluginPanelLocation::Sidebar => {
                        self.sidebar_visible = true;
                        if focus {
                            self.focus = Focus::Explorer;
                            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                                if let Some(id) = runtime
                                    .registration()
                                    .panels
                                    .into_iter()
                                    .find(|panel| panel.location == PluginPanelLocation::Sidebar)
                                    .map(|panel| panel.id)
                                {
                                    let _ = runtime.focus_autofocus_input(&id);
                                }
                            }
                        }
                    }
                    PluginPanelLocation::Bottom => {
                        self.bottom_visible = true;
                        self.bottom_tab = BottomTab::Output;
                        if focus {
                            self.focus = Focus::Output;
                            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                                if let Some(id) = runtime
                                    .registration()
                                    .panels
                                    .into_iter()
                                    .find(|panel| panel.location == PluginPanelLocation::Bottom)
                                    .map(|panel| panel.id)
                                {
                                    let _ = runtime.focus_autofocus_input(&id);
                                }
                            }
                        }
                    }
                },
                PluginPanelRequest::OpenDrawer {
                    side,
                    width,
                    min_width,
                } => {
                    self.plugin_drawer = Some(PluginDrawerState {
                        runtime_index,
                        side,
                        width,
                        min_width,
                    });
                }
                PluginPanelRequest::CloseDrawer => {
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        runtime.clear_drawer_state();
                    }
                    if self
                        .plugin_drawer
                        .as_ref()
                        .is_some_and(|drawer| drawer.runtime_index == runtime_index)
                    {
                        self.plugin_drawer = None;
                    }
                }
                PluginPanelRequest::OpenTab {
                    id,
                    title,
                    file_path,
                    focus,
                } => {
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        if let Ok(Some(text)) = runtime.render_panel(&format!("__tab:{id}")) {
                            self.git_diff_tab_text = None;
                            self.plugin_diff_file_path = None;
                            self.plugin_file_view = Some((title, text));
                            self.plugin_file_view_path = file_path.map(PathBuf::from);
                            self.plugin_custom_tab = Some((runtime_index, id.clone()));
                            self.plugin_tab_scroll = 0;
                            self.plugin_tab_hunk = 0;
                            self.plugin_tab_split = false;
                            self.plugin_tab_collapsed_hunks.clear();
                            if focus {
                                self.focus = Focus::Editor;
                                let _ = runtime.focus_autofocus_input(&format!("__tab:{id}"));
                            }
                        } else {
                            self.status_message = "Plugin tab render failed".to_owned();
                        }
                    }
                }
                PluginPanelRequest::OpenChangesTab {
                    id,
                    title,
                    file_path,
                    focus,
                } => {
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        if let Ok(Some(text)) = runtime.render_panel(&format!("__tab:{id}")) {
                            self.git_diff_tab_text = None;
                            self.plugin_diff_file_path = file_path.map(PathBuf::from);
                            self.plugin_file_view = Some((title, text));
                            self.plugin_file_view_path = None;
                            self.plugin_custom_tab = Some((runtime_index, id));
                            self.plugin_tab_scroll = 0;
                            self.plugin_tab_hunk = 0;
                            self.plugin_tab_split = false;
                            self.plugin_tab_collapsed_hunks.clear();
                            if focus {
                                self.focus = Focus::Editor;
                            }
                        } else {
                            self.status_message = "Plugin changes tab render failed".to_owned();
                        }
                    }
                }
                PluginPanelRequest::CloseTab { id } => {
                    // Release callbacks for background tabs as well as the
                    // currently visible tab; otherwise Lua registry values
                    // and event handlers survive until runtime shutdown.
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        if let Err(error) = runtime.close_tab(&id) {
                            self.status_message = format!("Plugin tab close failed: {error}");
                        }
                    }
                    if self
                        .plugin_custom_tab
                        .as_ref()
                        .is_some_and(|(owner, current)| *owner == runtime_index && current == &id)
                    {
                        self.plugin_custom_tab = None;
                        self.plugin_file_view = None;
                        self.plugin_file_view_path = None;
                        self.plugin_tab_scroll = 0;
                        self.plugin_tab_hunk = 0;
                        self.plugin_tab_split = false;
                        self.plugin_tab_collapsed_hunks.clear();
                        self.focus = Focus::Editor;
                        self.emit_plugin_event("tab.close");
                        self.emit_plugin_event("tab.activate");
                    }
                }
                PluginPanelRequest::FocusTab { id } => {
                    if self
                        .plugin_custom_tab
                        .as_ref()
                        .is_some_and(|(owner, current)| *owner == runtime_index && current == &id)
                    {
                        self.focus = Focus::Editor;
                        if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                            let _ = runtime.focus_autofocus_input(&format!("__tab:{id}"));
                        }
                    }
                }
                PluginPanelRequest::OpenFile {
                    path,
                    line,
                    readonly,
                } => {
                    let path = PathBuf::from(path);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        self.search_root.join(path)
                    };
                    if !path.starts_with(&self.search_root) {
                        self.status_message = "Plugin diff path is outside workspace".to_owned();
                        return;
                    }
                    match Buffer::open(&path) {
                        Ok(mut buffer) => {
                            buffer.set_readonly(readonly);
                            buffer.set_cursor(line.unwrap_or(1).saturating_sub(1), 0);
                            self.open_buffer(buffer);
                            self.sync_lsp_for_buffer();
                            self.emit_plugin_event("document.open");
                            self.focus = Focus::Editor;
                            self.status_message = format!(
                                "Opened {}{}",
                                path.display(),
                                if readonly { " (read-only)" } else { "" }
                            );
                        }
                        Err(error) => {
                            self.status_message = format!("Plugin open failed: {error}");
                        }
                    }
                }
                PluginPanelRequest::FocusFileView => {
                    if self.plugin_file_view.is_some() {
                        self.focus = Focus::Editor;
                    }
                }
                PluginPanelRequest::CloseFileView => {
                    if self.plugin_file_view.is_some() {
                        self.close_plugin_file_view();
                    }
                }
                PluginPanelRequest::RefreshFileView { id } => {
                    if self
                        .plugin_file_close
                        .is_some_and(|(owner, current)| owner == runtime_index && current == id)
                    {
                        if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                            match runtime.render_file_view(id) {
                                Ok(Some(text)) => {
                                    if let Some((_, current_text)) = self.plugin_file_view.as_mut()
                                    {
                                        *current_text = text;
                                        let max_scroll =
                                            current_text.lines().count().saturating_sub(1);
                                        self.plugin_tab_scroll =
                                            self.plugin_tab_scroll.min(max_scroll);
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    self.status_message =
                                        format!("Plugin file redraw failed: {error}");
                                }
                            }
                        }
                    }
                }
                PluginPanelRequest::RefreshPanel { id: _ } => {}
                PluginPanelRequest::ClipboardWrite { text } => {
                    self.clipboard = text;
                    self.queue_osc52_clipboard();
                    self.sync_plugin_clipboard();
                    self.status_message = "Plugin copied text to clipboard".to_owned();
                }
                PluginPanelRequest::Confirm { id, message } => {
                    self.begin_overlay();
                    self.plugin_dialog = Some(PluginDialog::Confirm {
                        runtime_index,
                        id,
                        message,
                    });
                }
                PluginPanelRequest::ShowInfo { title, entries } => {
                    self.begin_overlay();
                    self.plugin_dialog = Some(PluginDialog::Info { title, entries });
                }
                PluginPanelRequest::OpenDialog {
                    id,
                    title,
                    width,
                    height,
                } => {
                    self.begin_overlay();
                    self.plugin_dialog = Some(PluginDialog::Custom {
                        runtime_index,
                        id,
                        title,
                        width,
                        height,
                    });
                }
                PluginPanelRequest::CloseDialog => {
                    if let Some(PluginDialog::Custom {
                        runtime_index, id, ..
                    }) = self.plugin_dialog.take()
                    {
                        if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                            if let Err(error) = runtime.close_custom_dialog(id) {
                                self.status_message =
                                    format!("Plugin dialog close failed: {error}");
                            }
                        }
                        self.restore_overlay_focus();
                    }
                }
                PluginPanelRequest::CommandLineShow { id, prefix, text } => {
                    if self.plugin_dialog.is_none()
                        && !self.plugin_palette
                        && !self.settings_overlay
                        && !self.quick_open
                        && !self.goto_mode
                    {
                        self.begin_overlay();
                        self.plugin_command_line = Some(PluginCommandLine {
                            runtime_index,
                            id,
                            prefix,
                            cursor: text.chars().count(),
                            text,
                        });
                    }
                }
                PluginPanelRequest::CommandLineHide => {
                    self.plugin_command_line = None;
                    self.restore_overlay_focus();
                }
                PluginPanelRequest::CommandLineSetText { text } => {
                    let Some(command_line) = self.plugin_command_line.as_mut() else {
                        continue;
                    };
                    command_line.text = text.clone();
                    command_line.cursor = command_line.text.chars().count();
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        if let Err(error) = runtime.command_line_change(&text) {
                            self.status_message = format!("Plugin command line failed: {error}");
                        }
                    }
                }
            }
        }
    }

    fn reload_theme_from_settings(&mut self) {
        let refreshed = self
            .settings_path
            .as_deref()
            .map(Settings::load_from_path)
            .unwrap_or_else(Settings::load);
        let Some(theme_name) = refreshed.theme.as_deref() else {
            self.status_message = "Settings reloaded · no theme selected".to_owned();
            return;
        };
        let theme_path = PathBuf::from(theme_name);
        let resolved = if theme_path.is_file() {
            Some(theme_path)
        } else {
            config_root(self.settings_path.as_deref())
                .map(|root| root.join("themes").join(format!("{theme_name}.json")))
                .filter(|path| path.is_file())
        };
        let Some(path) = resolved else {
            self.status_message = format!("Theme not found: {theme_name}");
            return;
        };
        match Theme::from_path(&path) {
            Ok(theme) => {
                if let Some(index) = self
                    .theme_options
                    .iter()
                    .position(|(name, _)| name == theme_name || name == &path.display().to_string())
                {
                    self.theme_options[index].1 = theme;
                    self.theme_index = index;
                } else {
                    self.theme_options.push((theme_name.to_owned(), theme));
                    self.theme_index = self.theme_options.len() - 1;
                }
                self.theme = theme;
                self.settings.theme = Some(theme_name.to_owned());
                self.status_message = format!("Theme reloaded: {theme_name}");
            }
            Err(error) => self.status_message = format!("Theme reload failed: {error}"),
        }
    }

    fn handle_plugin_command_line_key(&mut self, key: KeyEvent) -> AppResult<()> {
        let Some(command_line) = self.plugin_command_line.as_ref() else {
            return Ok(());
        };
        let runtime_index = command_line.runtime_index;
        let key_name = key_chord(key).unwrap_or_else(|| format!("{:?}", key.code));
        let rune = match key.code {
            KeyCode::Char(value) => Some(value),
            _ => None,
        };
        let modifier = format_key_modifier(key.modifiers);
        if self
            .plugin_runtimes
            .get(runtime_index)
            .is_some_and(|runtime| {
                runtime
                    .command_line_key(&key_name, rune, &modifier)
                    .unwrap_or(false)
            })
        {
            return Ok(());
        }
        match key.code {
            KeyCode::Enter => {
                let text = self
                    .plugin_command_line
                    .take()
                    .map(|command_line| command_line.text)
                    .unwrap_or_default();
                self.restore_overlay_focus();
                if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                    runtime.command_line_finish(&text, true)?;
                }
            }
            KeyCode::Esc => {
                self.plugin_command_line = None;
                self.restore_overlay_focus();
                if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                    runtime.command_line_finish("", false)?;
                }
            }
            KeyCode::Backspace => {
                self.edit_plugin_command_line("Backspace", None)?;
            }
            KeyCode::Left => self.edit_plugin_command_line("Left", None)?,
            KeyCode::Right => self.edit_plugin_command_line("Right", None)?,
            KeyCode::Home => self.edit_plugin_command_line("Home", None)?,
            KeyCode::End => self.edit_plugin_command_line("End", None)?,
            KeyCode::Delete => self.edit_plugin_command_line("Delete", None)?,
            KeyCode::Char(value) if key.modifiers.is_empty() => {
                self.edit_plugin_command_line("Char", Some(value))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn edit_plugin_command_line(&mut self, key_name: &str, rune: Option<char>) -> AppResult<()> {
        let Some(command_line) = self.plugin_command_line.as_mut() else {
            return Ok(());
        };
        let changed = edit_command_line(command_line, key_name, rune).unwrap_or(false);
        if changed {
            let text = command_line.text.clone();
            let runtime_index = command_line.runtime_index;
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                runtime.command_line_change(&text)?;
            }
        }
        Ok(())
    }

    fn close_plugin_dialog(&mut self, confirmed: bool) -> AppResult<()> {
        let Some(dialog) = self.plugin_dialog.take() else {
            return Ok(());
        };
        if let PluginDialog::Confirm {
            runtime_index, id, ..
        } = dialog
        {
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                runtime.resolve_dialog(id, confirmed)?;
            }
        } else if let PluginDialog::Custom {
            runtime_index, id, ..
        } = dialog
        {
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                runtime.close_custom_dialog(id)?;
            }
        }
        self.restore_overlay_focus();
        Ok(())
    }
    fn sync_plugin_clipboard(&self) {
        for runtime in &self.plugin_runtimes {
            runtime.set_clipboard_text(&self.clipboard);
        }
    }
    fn refresh_plugin_custom_tab(&mut self) {
        let Some((runtime_index, id)) = self.plugin_custom_tab.as_ref() else {
            return;
        };
        let Some(runtime) = self.plugin_runtimes.get(*runtime_index) else {
            return;
        };
        let rendered = runtime.render_panel(&format!("__tab:{id}"));
        if let Ok(Some(text)) = rendered {
            self.plugin_tab_scroll = self
                .plugin_tab_scroll
                .min(text.lines().count().saturating_sub(1));
            if let Some((_, current_text)) = self.plugin_file_view.as_mut() {
                let text = collapse_plugin_diff_hunks(&text, &self.plugin_tab_collapsed_hunks);
                *current_text = if self.plugin_tab_split {
                    split_unified_diff_text(&text)
                } else {
                    text
                };
            }
        }
    }

    fn open_git_diff_tab(&mut self) {
        let text = self
            .change_diff
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\r\n");
        self.plugin_custom_tab = None;
        self.git_diff_tab_text = Some(text);
        self.plugin_file_view = Some(("Git diff".to_owned(), String::new()));
        self.plugin_file_view_path = None;
        self.plugin_tab_scroll = 0;
        self.plugin_tab_hunk = 0;
        self.plugin_tab_split = true;
        self.plugin_tab_collapsed_hunks.clear();
        self.refresh_git_diff_tab();
        self.focus = Focus::Editor;
        self.status_message =
            "Git diff tab · v unified/split · Enter edit file · Esc close".to_owned();
    }

    fn open_commit_detail(&mut self) {
        if self.commit_detail_pending {
            self.status_message = "Commit details are still loading".to_owned();
            return;
        }
        let Some(commit) = self.history.get(self.output_scroll).cloned() else {
            self.status_message = "Select a commit to inspect".to_owned();
            return;
        };
        let Some(root) = self.history_root.clone() else {
            self.status_message = "Git history repository is unavailable".to_owned();
            return;
        };
        self.commit_detail = Some((commit.clone(), root.clone(), String::new()));
        self.blame_lines.clear();
        self.plugin_registry_entries.clear();
        self.pull_request_comments.clear();
        self.output_scroll = 0;
        self.start_commit_detail_load(commit, root);
    }

    fn start_commit_detail_load(&mut self, commit: GitCommit, root: PathBuf) {
        self.commit_detail_generation.fetch_add(1, Ordering::AcqRel);
        self.commit_detail_pending = true;
        let generation = self.commit_detail_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation_state = Arc::clone(&self.commit_detail_generation);
        let sender = self.commit_detail_tx.clone();
        let worker_root = root.clone();
        let hash = commit.hash.clone();
        let context_lines = if self.diff_full_context { 3 } else { 0 };
        if std::thread::Builder::new()
            .name("mtide-git-commit-detail".to_owned())
            .spawn(move || {
                let result = git_show_commit(&worker_root, &hash, context_lines);
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(GitCommitDetailCompletion {
                        generation,
                        root: worker_root,
                        hash,
                        result,
                    });
                }
            })
            .is_err()
        {
            self.commit_detail_pending = false;
            self.status_message = "Unable to start Git commit details".to_owned();
        } else {
            self.status_message = format!(
                "Loading commit {}…",
                &commit.hash[..commit.hash.len().min(8)]
            );
        }
    }

    fn close_commit_detail(&mut self) {
        self.commit_detail_generation.fetch_add(1, Ordering::AcqRel);
        self.commit_detail_pending = false;
        self.commit_detail = None;
        self.output_scroll = 0;
        self.status_message = "Returned to Git history".to_owned();
    }

    fn reload_commit_detail_context(&mut self) {
        let Some((commit, root, _)) = self.commit_detail.clone() else {
            return;
        };
        self.start_commit_detail_load(commit, root);
        self.status_message = if self.diff_full_context {
            "Loading full commit context…".to_owned()
        } else {
            "Loading commit changes only…".to_owned()
        };
    }

    fn refresh_git_diff_tab(&mut self) {
        if self.plugin_custom_tab.is_some() {
            return;
        }
        let Some(source) = self.git_diff_tab_text.as_deref() else {
            return;
        };
        let rendered = if self.plugin_tab_split {
            split_unified_diff_text(source)
        } else {
            source.to_owned()
        };
        if let Some((_, text)) = self.plugin_file_view.as_mut() {
            *text = rendered;
        }
    }
    pub fn resize_terminal(&mut self, columns: u16, rows: u16) -> AppResult<()> {
        self.automation_size.set((columns.max(1), rows.max(1)));
        for terminal in &mut self.terminals {
            terminal.resize(columns, rows)?;
        }
        self.agent.resize(columns, rows)
    }
    pub fn plugin_summary(&self) -> Option<(usize, usize, usize, usize)> {
        if self.plugin_runtimes.is_empty() {
            return None;
        }
        let (commands, keybindings, panels) = self
            .plugin_runtimes
            .iter()
            .map(PluginRuntime::registration)
            .fold(
                (0, 0, 0),
                |(commands, keybindings, panels), registration| {
                    (
                        commands + registration.commands.len(),
                        keybindings + registration.keybindings.len(),
                        panels + registration.panels.len(),
                    )
                },
            );
        Some((self.plugin_runtimes.len(), commands, keybindings, panels))
    }
    pub fn plugin_registry_entries(&self) -> &[PluginRegistryEntry] {
        &self.plugin_registry_entries
    }
    pub fn plugin_status_items_detailed(&self) -> (Vec<PluginStatusEntry>, Vec<PluginStatusEntry>) {
        let mut items = self
            .plugin_runtimes
            .iter()
            .enumerate()
            .flat_map(|(runtime_index, runtime)| {
                runtime
                    .registration()
                    .status_items
                    .into_iter()
                    .map(move |item| (runtime_index, item))
            })
            .collect::<Vec<_>>();
        items.sort_by_key(|(_, item)| item.priority);
        let mut left = Vec::new();
        let mut right = Vec::new();
        for (runtime_index, item) in items {
            let target = if item.side == "right" {
                &mut right
            } else {
                &mut left
            };
            target.push((runtime_index, item.id, item.text));
        }
        (left, right)
    }
    pub fn plugin_echo(&self) -> Option<String> {
        self.plugin_runtimes
            .iter()
            .rev()
            .map(PluginRuntime::echo_text)
            .find(|text| !text.is_empty())
    }
    pub fn plugin_notification(&self) -> Option<(String, String)> {
        self.plugin_runtimes
            .iter()
            .rev()
            .filter_map(|runtime| runtime.notification())
            .next()
    }
    pub fn set_plugin_status_hits(&self, hits: Vec<(u16, u16, usize, String)>) {
        *self.plugin_status_hits.borrow_mut() = hits
            .into_iter()
            .map(|(start, end, runtime_index, id)| PluginStatusHit {
                start,
                end,
                runtime_index,
                id,
            })
            .collect();
    }
    pub fn activate_plugin_status_at(&mut self, column: u16) -> AppResult<bool> {
        let hit = self
            .plugin_status_hits
            .borrow()
            .iter()
            .find(|hit| column >= hit.start && column < hit.end)
            .cloned();
        let Some(hit) = hit else {
            return Ok(false);
        };
        let Some(runtime) = self.plugin_runtimes.get(hit.runtime_index) else {
            return Ok(false);
        };
        let activated = runtime.activate_status_item(&hit.id)?;
        if activated {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            self.status_message = format!("Plugin status activated · {}", hit.id);
        }
        Ok(activated)
    }
    pub const fn theme(&self) -> Theme {
        self.theme
    }
    pub fn theme_name(&self) -> &str {
        self.theme_options
            .get(self.theme_index)
            .map_or("Built-in", |(name, _)| name.as_str())
    }
    fn cycle_theme(&mut self) {
        if self.theme_options.len() < 2 {
            self.status_message = "No plugin themes available".to_owned();
            return;
        }
        self.theme_index = (self.theme_index + 1) % self.theme_options.len();
        self.theme = self.theme_options[self.theme_index].1;
        self.status_message = format!("Theme: {}", self.theme_name());
    }
    pub fn plugin_panel_text_sized(
        &self,
        location: PluginPanelLocation,
        width: u16,
        height: u16,
    ) -> Option<String> {
        self.plugin_panel_text_at(location, width, height)
    }
    pub fn set_plugin_panel_line_start(&self, location: PluginPanelLocation, line: Option<usize>) {
        match location {
            PluginPanelLocation::Sidebar => self.plugin_sidebar_line_start.set(line),
            PluginPanelLocation::Bottom => self.plugin_bottom_line_start.set(line),
        }
    }
    pub fn plugin_panel_line_start(&self, location: PluginPanelLocation) -> Option<usize> {
        match location {
            PluginPanelLocation::Sidebar => self.plugin_sidebar_line_start.get(),
            PluginPanelLocation::Bottom => self.plugin_bottom_line_start.get(),
        }
    }
    pub fn activate_plugin_panel_selection(
        &mut self,
        location: PluginPanelLocation,
        row: usize,
    ) -> AppResult<bool> {
        let Some((runtime_index, id)) =
            self.plugin_runtimes
                .iter()
                .enumerate()
                .find_map(|(runtime_index, runtime)| {
                    runtime
                        .registration()
                        .panels
                        .into_iter()
                        .find(|panel| panel.location == location)
                        .map(|panel| (runtime_index, panel.id))
                })
        else {
            return Ok(false);
        };
        let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
            return Ok(false);
        };
        let _ = runtime.focus_panel_dropdown(&id, row);
        let _ = runtime.focus_panel_radio(&id, row);
        let _ = runtime.focus_panel_tabs(&id, row);
        let _ = runtime.focus_panel_scrollview(&id, row);
        if runtime.activate_panel_checkbox(&id, row)? {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            return Ok(true);
        }
        if runtime.focus_panel_input(&id, row) {
            self.apply_plugin_editor_requests();
            return Ok(true);
        }
        if runtime.focus_panel_password(&id, row) {
            self.apply_plugin_editor_requests();
            return Ok(true);
        }
        if runtime.activate_panel_button(&id, row)? {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            self.status_message = format!("Plugin button activated · {}", row + 1);
            return Ok(true);
        }
        let activated = runtime.activate_panel_selection(&id, row)?;
        if activated {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            self.status_message = format!("Plugin row activated · {}", row + 1);
        }
        Ok(activated)
    }
    fn dispatch_plugin_panel_key(&mut self, key: KeyEvent) -> AppResult<bool> {
        let location = match self.focus {
            Focus::Explorer => PluginPanelLocation::Sidebar,
            Focus::Terminal | Focus::Output => PluginPanelLocation::Bottom,
            _ => return Ok(false),
        };
        if matches!(
            (key.modifiers, key.code),
            (KeyModifiers::SHIFT, KeyCode::Enter)
        ) {
            return self.open_selected_plugin_panel_context_menu(location);
        }
        let key_name = key_chord(key).unwrap_or_else(|| format!("{:?}", key.code));
        let rune = match key.code {
            KeyCode::Char(character) => Some(character),
            _ => None,
        };
        let modifier = format_key_modifier(key.modifiers);
        for runtime in &self.plugin_runtimes {
            let Some(panel_id) = runtime
                .registration()
                .panels
                .into_iter()
                .find(|panel| panel.location == location)
                .map(|panel| panel.id)
            else {
                continue;
            };
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                let backwards = key.modifiers.contains(KeyModifiers::SHIFT)
                    || matches!(key.code, KeyCode::BackTab);
                if runtime.cycle_panel_focus(&panel_id, backwards) {
                    self.apply_plugin_editor_requests();
                    return Ok(true);
                }
            }
            if runtime.panel_checkbox_key(&panel_id, &key_name, rune)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if runtime.panel_dropdown_key(&panel_id, &key_name)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if runtime.panel_radio_key(&panel_id, &key_name)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if runtime.panel_tabs_key(&panel_id, &key_name)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if runtime.panel_scrollview_key(&panel_id, &key_name)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if runtime.panel_button_key(&panel_id, &key_name, rune)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if runtime.panel_input_key(&panel_id, &key_name, rune, &modifier)? {
                self.apply_plugin_editor_requests();
                return Ok(true);
            }
            if runtime.panel_password_key(&panel_id, &key_name, rune, &modifier)? {
                self.apply_plugin_editor_requests();
                return Ok(true);
            }
            if runtime.emit_panel_key_event(&panel_id, &key_name, rune, &modifier)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
            if key.modifiers.is_empty() {
                if let KeyCode::Char(character) = key.code {
                    if runtime.activate_panel_command(&panel_id, &character.to_string())? {
                        self.apply_plugin_editor_requests();
                        self.apply_plugin_bookmark_events();
                        return Ok(true);
                    }
                }
            }
            let activated = match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => runtime.move_panel_selection(&panel_id, -1)?,
                (KeyModifiers::NONE, KeyCode::Down) => {
                    runtime.move_panel_selection(&panel_id, 1)?
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    runtime.move_panel_selection(&panel_id, -8)?
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    runtime.move_panel_selection(&panel_id, 8)?
                }
                (KeyModifiers::NONE, KeyCode::Left) => {
                    runtime.move_tree_horizontal_selection(&panel_id, false)?
                        || runtime.move_panel_horizontal_selection(&panel_id, -1)?
                }
                (KeyModifiers::NONE, KeyCode::Right) => {
                    runtime.move_tree_horizontal_selection(&panel_id, true)?
                        || runtime.move_panel_horizontal_selection(&panel_id, 1)?
                }
                (KeyModifiers::NONE, KeyCode::Home) => {
                    runtime.move_panel_selection(&panel_id, isize::MIN)?
                }
                (KeyModifiers::NONE, KeyCode::End) => {
                    runtime.move_panel_selection(&panel_id, isize::MAX)?
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    runtime.activate_panel_current_selection(&panel_id)?
                }
                _ => false,
            };
            if activated {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn emit_plugin_panel_mouse(
        &mut self,
        location: PluginPanelLocation,
        x: u16,
        y: u16,
        button: &str,
    ) -> AppResult<bool> {
        let Some((runtime_index, panel_id)) =
            self.plugin_runtimes
                .iter()
                .enumerate()
                .find_map(|(runtime_index, runtime)| {
                    runtime
                        .registration()
                        .panels
                        .into_iter()
                        .find(|panel| panel.location == location)
                        .map(|panel| (runtime_index, panel.id))
                })
        else {
            return Ok(false);
        };
        if button == "left"
            && self.plugin_runtimes[runtime_index].activate_panel_action(
                &panel_id,
                usize::from(x),
                usize::from(y),
            )?
        {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            return Ok(true);
        }
        if button == "left"
            && self.plugin_runtimes[runtime_index]
                .activate_panel_control_at(&panel_id, usize::from(y))?
        {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            return Ok(true);
        }
        let handled =
            self.plugin_runtimes[runtime_index].emit_panel_mouse_event(&panel_id, x, y, button)?;
        if handled {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
        }
        Ok(handled)
    }

    pub fn emit_plugin_tab_mouse(&mut self, x: u16, y: u16, button: &str) -> AppResult<bool> {
        let Some((runtime_index, tab_id)) = self.plugin_custom_tab.as_ref().cloned() else {
            return Ok(false);
        };
        let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
            return Ok(false);
        };
        let panel_id = format!("__tab:{tab_id}");
        let mut handled = false;
        if button == "left" {
            handled = runtime.activate_panel_action(&panel_id, usize::from(x), usize::from(y))?
                || runtime.activate_panel_control_at(&panel_id, usize::from(y))?;
        }
        if !handled {
            handled = runtime.emit_tab_mouse_event(&tab_id, x, y, button)?;
        }
        if handled {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            self.refresh_plugin_custom_tab();
        }
        Ok(handled)
    }

    pub fn emit_plugin_file_mouse(&mut self, x: u16, y: u16, button: &str) -> AppResult<bool> {
        let Some((runtime_index, file_id)) = self.plugin_file_close.as_ref().cloned() else {
            return Ok(false);
        };
        let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
            return Ok(false);
        };
        let panel_id = format!("__file:{file_id}");
        let mut handled = false;
        if button == "left" {
            handled = runtime.activate_panel_action(&panel_id, usize::from(x), usize::from(y))?
                || runtime.activate_panel_control_at(&panel_id, usize::from(y))?;
        }
        if !handled {
            handled = runtime.emit_file_mouse_event(file_id, x, y, button)?;
        }
        if handled {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            self.refresh_plugin_custom_tab();
        }
        Ok(handled)
    }

    pub fn plugin_drawer(&self) -> Option<(&str, u16, u16, String)> {
        let drawer = self.plugin_drawer.as_ref()?;
        let runtime = self.plugin_runtimes.get(drawer.runtime_index)?;
        let text = runtime.render_panel("__drawer").ok().flatten()?;
        Some((drawer.side.as_str(), drawer.width, drawer.min_width, text))
    }

    pub fn plugin_drawer_text_sized(&self, width: u16, height: u16) -> Option<String> {
        let drawer = self.plugin_drawer.as_ref()?;
        let runtime = self.plugin_runtimes.get(drawer.runtime_index)?;
        runtime.set_panel_size(width, height);
        runtime.render_panel("__drawer").ok().flatten()
    }

    pub fn activate_plugin_drawer_line(&mut self, line: usize) -> AppResult<bool> {
        let Some(drawer) = self.plugin_drawer.clone() else {
            return Ok(false);
        };
        let Some(runtime) = self.plugin_runtimes.get(drawer.runtime_index) else {
            return Ok(false);
        };
        let activated = runtime.activate_drawer_line(line)?;
        if activated {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
        }
        Ok(activated)
    }

    pub fn focus_plugin_drawer_line(&mut self, line: usize) -> bool {
        let Some(drawer) = self.plugin_drawer.clone() else {
            return false;
        };
        self.plugin_runtimes
            .get(drawer.runtime_index)
            .is_some_and(|runtime| runtime.focus_drawer_line(line))
    }

    fn handle_plugin_drawer_key(&mut self, key: KeyEvent) -> AppResult<bool> {
        let Some(drawer) = self.plugin_drawer.clone() else {
            return Ok(false);
        };
        let key_name = key_chord(key).unwrap_or_else(|| format!("{:?}", key.code));
        let rune = match key.code {
            KeyCode::Char(character) => Some(character),
            _ => None,
        };
        let modifier = format_key_modifier(key.modifiers);
        let Some(runtime) = self.plugin_runtimes.get(drawer.runtime_index) else {
            return Ok(false);
        };
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let backwards =
                key.modifiers.contains(KeyModifiers::SHIFT) || matches!(key.code, KeyCode::BackTab);
            if runtime.cycle_drawer_focus(backwards) {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
        }
        let handled = runtime.drawer_input_key(&key_name, rune, &modifier)?
            || runtime.drawer_password_key(&key_name, rune, &modifier)?;
        if handled {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
        }
        Ok(handled)
    }
    fn plugin_panel_text_at(
        &self,
        location: PluginPanelLocation,
        width: u16,
        height: u16,
    ) -> Option<String> {
        self.plugin_runtimes.iter().find_map(|runtime| {
            let panel_id = runtime
                .registration()
                .panels
                .into_iter()
                .find(|panel| panel.location == location)?
                .id;
            runtime.set_panel_size(width, height);
            runtime.render_panel(&panel_id).ok().flatten()
        })
    }
    pub fn plugin_logs(&self) -> Vec<String> {
        self.plugin_runtimes
            .iter()
            .flat_map(PluginRuntime::logs)
            .collect()
    }
    pub fn plugin_file_view(&self) -> Option<(&str, &str)> {
        self.plugin_file_view
            .as_ref()
            .map(|(title, text)| (title.as_str(), text.as_str()))
    }
    pub fn plugin_diff_view_active(&self) -> bool {
        self.git_diff_tab_text.is_some()
            || self.plugin_diff_file_path.is_some()
            || self.plugin_file_view.as_ref().is_some_and(|(_, text)| {
                text.lines()
                    .any(|line| line.starts_with("diff --git ") || line.starts_with("@@"))
            })
    }
    pub const fn plugin_custom_tab_active(&self) -> bool {
        self.plugin_custom_tab.is_some()
    }
    pub fn plugin_file_view_text_sized(&self, width: u16, height: u16) -> Option<String> {
        if let Some((runtime_index, id)) = self.plugin_custom_tab.as_ref() {
            let runtime = self.plugin_runtimes.get(*runtime_index)?;
            runtime.set_panel_size(width, height);
            let text = runtime
                .render_panel(&format!("__tab:{id}"))
                .ok()
                .flatten()?;
            let text = collapse_plugin_diff_hunks(&text, &self.plugin_tab_collapsed_hunks);
            Some(if self.plugin_tab_split {
                split_unified_diff_text(&text)
            } else {
                text
            })
        } else {
            self.plugin_file_view.as_ref().map(|(_, text)| text.clone())
        }
    }
    pub const fn plugin_file_view_scroll(&self) -> usize {
        self.plugin_tab_scroll
    }
    pub fn scroll_plugin_file_view(&mut self, delta: isize) {
        let line_count = self
            .plugin_file_view
            .as_ref()
            .map_or(0, |(_, text)| text.lines().count());
        self.plugin_tab_scroll = if delta.is_negative() {
            self.plugin_tab_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.plugin_tab_scroll
                .saturating_add(delta as usize)
                .min(line_count.saturating_sub(1))
        };
        self.plugin_view_hover_link = None;
    }
    pub fn plugin_file_view_hover_link(&self, line: usize) -> Option<&str> {
        self.plugin_view_hover_link
            .as_ref()
            .filter(|(hover_line, _)| *hover_line == line)
            .map(|(_, link)| link.as_str())
    }

    pub fn update_plugin_file_view_hover(
        &mut self,
        visual_row: usize,
        visual_column: usize,
        width: u16,
        height: u16,
        control: bool,
    ) {
        self.plugin_view_hover_link = None;
        if !control || !self.plugin_diff_view_active() {
            return;
        }
        let Some(text) = self.plugin_file_view_text_sized(width, height) else {
            return;
        };
        let Some((line, column)) = plugin_text_position(
            &text,
            width as usize,
            self.plugin_tab_scroll.saturating_add(visual_row),
            visual_column,
        ) else {
            return;
        };
        let Some(reference) = text
            .lines()
            .nth(line)
            .and_then(|text_line| editor_link_at(text_line, column))
        else {
            return;
        };
        let valid = if reference.starts_with("http://") || reference.starts_with("https://") {
            valid_http_url(&reference)
        } else {
            parse_editor_file_reference(&reference).is_some()
        };
        if valid {
            self.plugin_view_hover_link = Some((line, reference));
        }
    }

    pub fn open_plugin_file_view_link_at(
        &mut self,
        visual_row: usize,
        visual_column: usize,
        width: u16,
        height: u16,
    ) -> bool {
        if !self.plugin_diff_view_active() {
            return false;
        }
        let Some(text) = self.plugin_file_view_text_sized(width, height) else {
            return false;
        };
        let Some((line, column)) = plugin_text_position(
            &text,
            width as usize,
            self.plugin_tab_scroll.saturating_add(visual_row),
            visual_column,
        ) else {
            return false;
        };
        let Some(reference) = text
            .lines()
            .nth(line)
            .and_then(|text_line| editor_link_at(text_line, column))
        else {
            return false;
        };
        if reference.starts_with("http://") || reference.starts_with("https://") {
            if !valid_http_url(&reference) {
                return false;
            }
            self.open_terminal_link(&reference);
        } else {
            let Some(target) = self
                .resolve_file_reference(&reference, self.plugin_file_view_source_path().as_deref())
            else {
                return false;
            };
            if let Err(error) = self.open_external_file_target(target) {
                self.status_message = format!("Unable to open reference: {error}");
            }
        }
        self.plugin_view_hover_link = None;
        true
    }

    fn plugin_file_view_source_path(&self) -> Option<PathBuf> {
        if let Some((_, root, text)) = &self.commit_detail {
            return commit_detail_source_path(root, text, self.plugin_tab_scroll);
        }
        self.plugin_diff_file_path
            .as_ref()
            .or(self.plugin_file_view_path.as_ref())
            .map(|path| {
                if path.is_absolute() {
                    return path.clone();
                }
                self.workspace_roots
                    .iter()
                    .map(|root| root.join(path))
                    .find(|candidate| candidate.is_file())
                    .unwrap_or_else(|| self.search_root.join(path))
            })
            .or_else(|| {
                self.git_diff_tab_text.as_ref().and_then(|_| {
                    let change = self.changes.get(self.change_index)?;
                    Some(self.change_root(self.change_index).join(&change.path))
                })
            })
    }
    pub fn lsp_server_count(&self) -> usize {
        self.lsp.server_count()
    }
    pub fn lsp_state(&self) -> Option<LspServerState> {
        self.lsp.state(self.language())
    }
    pub const fn lsp_popup(&self) -> Option<&LspPopup> {
        self.lsp_popup.as_ref()
    }
    pub const fn rename_mode(&self) -> bool {
        self.rename_mode
    }
    pub fn rename_text(&self) -> &str {
        &self.rename_text
    }
    pub const fn plugin_palette_open(&self) -> bool {
        self.plugin_palette
    }
    pub fn plugin_dialog(&self) -> Option<&PluginDialog> {
        self.plugin_dialog.as_ref()
    }
    pub fn plugin_dialog_content(&self) -> Option<String> {
        let Some(PluginDialog::Custom {
            runtime_index, id, ..
        }) = self.plugin_dialog.as_ref()
        else {
            return None;
        };
        self.plugin_runtimes
            .get(*runtime_index)
            .and_then(|runtime| {
                runtime
                    .render_panel(&format!("__dialog:{id}"))
                    .ok()
                    .flatten()
            })
    }
    pub fn plugin_dialog_mouse_click(&mut self, line: usize) -> AppResult<()> {
        let Some(PluginDialog::Custom {
            runtime_index, id, ..
        }) = self.plugin_dialog.clone()
        else {
            return Ok(());
        };
        if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
            let focused = runtime.dialog_focus_at_line(id, line)
                || (line >= 2 && runtime.dialog_focus_button(id));
            if focused {
                runtime.dialog_activate(id)?;
            }
        }
        Ok(())
    }
    pub fn plugin_command_line(&self) -> Option<&PluginCommandLine> {
        self.plugin_command_line.as_ref()
    }
    pub fn plugin_context_menu(&self) -> Option<&PluginContextMenu> {
        self.plugin_context_menu.as_ref()
    }
    pub fn open_plugin_context_menu(&mut self, position: Position) -> AppResult<()> {
        let line = self
            .file_lines()
            .get(position.line)
            .cloned()
            .unwrap_or_default();
        let chars: Vec<char> = line.chars().collect();
        let column = position.column.min(chars.len());
        let mut start = column;
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let mut end = column;
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        let word: String = chars[start..end].iter().collect();
        for (runtime_index, runtime) in self.plugin_runtimes.iter().enumerate() {
            let items = runtime.editor_context_menu(position.line, position.column, &word)?;
            if !items.is_empty() {
                self.begin_overlay();
                self.plugin_context_menu = Some(PluginContextMenu {
                    runtime_index,
                    items,
                    selected: 0,
                    panel_id: None,
                    panel_row: None,
                });
                self.focus = Focus::Editor;
                return Ok(());
            }
        }
        Ok(())
    }
    pub fn close_plugin_context_menu(&mut self) {
        if let Some(menu) = self.plugin_context_menu.take() {
            if let Some(runtime) = self.plugin_runtimes.get(menu.runtime_index) {
                if menu.panel_id.is_none() {
                    runtime.clear_editor_context_menu();
                }
            }
        }
        self.restore_overlay_focus();
    }

    pub fn open_plugin_panel_context_menu(
        &mut self,
        location: PluginPanelLocation,
        _x: u16,
        y: u16,
    ) -> AppResult<bool> {
        let Some((runtime_index, panel_id, items)) = self
            .plugin_runtimes
            .iter()
            .enumerate()
            .find_map(|(runtime_index, runtime)| {
                let panel_id = runtime
                    .registration()
                    .panels
                    .into_iter()
                    .find(|panel| panel.location == location)
                    .map(|panel| panel.id)?;
                let items = runtime.panel_context_menu(&panel_id);
                (!items.is_empty()).then_some((runtime_index, panel_id, items))
            })
        else {
            return Ok(false);
        };
        let panel_row = self
            .plugin_panel_line_start(location)
            .map(|start| usize::from(y).saturating_sub(start));
        if let Some(row) = panel_row {
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                runtime.set_panel_selected_row(&panel_id, row);
            }
        }
        self.begin_overlay();
        self.plugin_context_menu = Some(PluginContextMenu {
            runtime_index,
            items,
            selected: 0,
            panel_id: Some(panel_id),
            panel_row,
        });
        Ok(true)
    }

    fn open_selected_plugin_panel_context_menu(
        &mut self,
        location: PluginPanelLocation,
    ) -> AppResult<bool> {
        let Some((runtime_index, panel_id, items, panel_row)) = self
            .plugin_runtimes
            .iter()
            .enumerate()
            .find_map(|(runtime_index, runtime)| {
                let panel_id = runtime
                    .registration()
                    .panels
                    .into_iter()
                    .find(|panel| panel.location == location)
                    .map(|panel| panel.id)?;
                let items = runtime.panel_context_menu(&panel_id);
                let panel_row = runtime.panel_selected_row(&panel_id).saturating_sub(1);
                (!items.is_empty()).then_some((runtime_index, panel_id, items, panel_row))
            })
        else {
            return Ok(false);
        };
        self.begin_overlay();
        self.plugin_context_menu = Some(PluginContextMenu {
            runtime_index,
            items,
            selected: 0,
            panel_id: Some(panel_id),
            panel_row: Some(panel_row),
        });
        Ok(true)
    }

    pub fn plugin_context_menu_click(&mut self, line: usize) -> AppResult<()> {
        let Some(menu) = self.plugin_context_menu.clone() else {
            return Ok(());
        };
        if line >= menu.items.len() {
            return Ok(());
        }
        self.plugin_context_menu = Some(PluginContextMenu {
            selected: line,
            ..menu
        });
        self.activate_plugin_context_menu()
    }
    fn activate_plugin_context_menu(&mut self) -> AppResult<()> {
        let Some(menu) = self.plugin_context_menu.clone() else {
            return Ok(());
        };
        let activated = if let Some(runtime) = self.plugin_runtimes.get(menu.runtime_index) {
            if let Some(panel_id) = menu.panel_id.as_deref() {
                let row = menu
                    .panel_row
                    .map_or_else(|| runtime.panel_selected_row(panel_id), |row| row + 1);
                runtime.activate_panel_context_item(panel_id, menu.selected, row)?
            } else {
                runtime.activate_editor_context_item(menu.selected)?
            }
        } else {
            false
        };
        self.close_plugin_context_menu();
        if activated {
            self.apply_plugin_editor_requests();
            self.status_message = "Plugin context action executed".to_owned();
        }
        Ok(())
    }
    fn handle_plugin_context_menu_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.plugin_context_menu.is_none() {
            return Ok(());
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(menu) = self.plugin_context_menu.as_mut() {
                    menu.selected = menu.selected.saturating_sub(1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(menu) = self.plugin_context_menu.as_mut() {
                    menu.selected = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => self.activate_plugin_context_menu()?,
            (KeyModifiers::NONE, KeyCode::Esc) => self.close_plugin_context_menu(),
            _ => {}
        }
        Ok(())
    }
    pub fn git_credential_prompt(&self) -> Option<&GitCredentialPrompt> {
        self.git_credential_prompt.as_ref()
    }
    pub const fn settings_overlay_open(&self) -> bool {
        self.settings_overlay
    }
    pub const fn settings_index(&self) -> usize {
        self.settings_index
    }
    pub fn settings_rows(&self) -> Vec<(String, bool)> {
        vec![
            ("Sidebar visible".to_owned(), self.sidebar_visible),
            ("Bottom panel visible".to_owned(), self.bottom_visible),
            ("Agent panel visible".to_owned(), self.agent_visible),
            (
                "Editor word wrap".to_owned(),
                self.settings.editor.word_wrap,
            ),
            (
                "Format on save".to_owned(),
                self.settings.editor.format_on_save,
            ),
            (
                "Save all on quit".to_owned(),
                self.settings.save_all_on_quit,
            ),
            (
                "Terminal Ctrl-V paste".to_owned(),
                self.settings.terminal.ctrl_v,
            ),
            ("LSP enabled".to_owned(), self.settings.lsp.enabled),
            (
                "Auto-detect indentation".to_owned(),
                self.auto_detect_indent,
            ),
            (
                "Use tabs for indentation".to_owned(),
                self.effective_use_tabs(),
            ),
            (
                format!("Indent size: {} (Left/Right)", self.effective_indent_size()),
                true,
            ),
            (
                "Quick Open hidden paths".to_owned(),
                self.settings.quick_open.show_hidden,
            ),
            (
                "Quick Open Git ignored".to_owned(),
                self.settings.quick_open.show_git_ignored,
            ),
            (
                "Explorer hidden paths".to_owned(),
                self.settings.explorer.show_hidden,
            ),
            (
                "Explorer Git ignored".to_owned(),
                self.settings.explorer.show_git_ignored,
            ),
            (
                "Explorer auto reveal".to_owned(),
                self.settings.explorer.auto_reveal,
            ),
            (
                format!("Shortcut modifier: {}", self.settings.keyboard.modifier_key),
                true,
            ),
            ("Auto save".to_owned(), self.settings.editor.auto_save),
            (
                format!(
                    "Auto-save delay: {}ms (Left/Right)",
                    self.settings.editor.auto_save_delay_ms
                ),
                true,
            ),
            (
                "Invert selection colors".to_owned(),
                self.settings
                    .editor
                    .selection_style
                    .eq_ignore_ascii_case("invert"),
            ),
            (
                "Diff word wrap".to_owned(),
                self.settings.editor.diff_word_wrap,
            ),
            (
                "Diff high contrast".to_owned(),
                self.settings.editor.diff_high_contrast,
            ),
        ]
    }
    pub const fn quick_open_open(&self) -> bool {
        self.quick_open
    }
    pub fn quick_open_query(&self) -> &str {
        &self.quick_open_query
    }
    pub fn quick_open_results(&self) -> &[PathBuf] {
        &self.quick_open_results
    }
    pub const fn quick_open_index(&self) -> usize {
        self.quick_open_index
    }
    pub fn workspace_roots(&self) -> &[PathBuf] {
        &self.workspace_roots
    }

    pub fn project_name(&self) -> String {
        self.workspace_roots
            .first()
            .and_then(|root| root.file_name())
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "workspace".to_owned())
    }
    pub fn quick_open_title(&self) -> &'static str {
        if self.quick_open_save_as {
            "Save As"
        } else if self.quick_open_save_workspace {
            "Save Workspace"
        } else if self.quick_open_add_folder {
            "Add Workspace Folder"
        } else if self.quick_open_query.starts_with('>') {
            "Command Palette"
        } else if self.quick_open_query.starts_with('?') {
            "Keyboard Help"
        } else {
            "Open File"
        }
    }
    pub const fn goto_mode(&self) -> bool {
        self.goto_mode
    }
    pub fn goto_input(&self) -> &str {
        &self.goto_input
    }
    pub fn plugin_palette_items(&self) -> Vec<(String, String)> {
        self.command_palette_items(&self.plugin_query)
    }
    fn command_palette_items(&self, query: &str) -> Vec<(String, String)> {
        let query = query.trim_start_matches('>').trim().to_lowercase();
        builtin_commands()
            .into_iter()
            .filter(|(id, title)| {
                query.is_empty()
                    || id.to_lowercase().contains(&query)
                    || title.to_lowercase().contains(&query)
            })
            .chain(self.plugin_runtimes.iter().flat_map(|runtime| {
                runtime
                    .registration()
                    .commands
                    .into_iter()
                    .filter(|command| {
                        query.is_empty()
                            || command.id.to_lowercase().contains(&query)
                            || command.title.to_lowercase().contains(&query)
                    })
                    .map(|command| (command.id, command.title))
            }))
            .collect()
    }
    pub const fn plugin_palette_index(&self) -> usize {
        self.plugin_index
    }
    pub fn plugin_palette_query(&self) -> &str {
        &self.plugin_query
    }
    pub fn command_palette_items_for_quick_open(&self) -> Vec<(String, String)> {
        self.command_palette_items(&self.quick_open_query)
    }
    pub fn quick_open_help_items(&self) -> Vec<(String, String)> {
        let query = self
            .quick_open_query
            .trim_start_matches('?')
            .trim()
            .to_lowercase();
        [
            ("Ctrl-P".to_owned(), "Open command palette".to_owned()),
            ("Ctrl-O".to_owned(), "Open file".to_owned()),
            ("Ctrl-Shift-S".to_owned(), "Save as a new file".to_owned()),
            ("Ctrl-G".to_owned(), "Go to line and column".to_owned()),
            ("Ctrl-F".to_owned(), "Find in workspace".to_owned()),
            ("Ctrl-R".to_owned(), "Toggle replace mode".to_owned()),
            ("Ctrl-K E".to_owned(), "Focus Explorer".to_owned()),
            ("Ctrl-K F".to_owned(), "Focus Find".to_owned()),
            ("Ctrl-K C".to_owned(), "Focus Changes".to_owned()),
            ("Ctrl-K T".to_owned(), "Open a new terminal".to_owned()),
            ("Ctrl-T".to_owned(), "Toggle terminal".to_owned()),
            ("Alt-T".to_owned(), "Toggle fullscreen terminal".to_owned()),
            (
                "Alt-Shift-Up/Down".to_owned(),
                "Resize bottom panel".to_owned(),
            ),
            ("Ctrl-S".to_owned(), "Save current file".to_owned()),
            ("Ctrl-W".to_owned(), "Close current tab".to_owned()),
            (
                "Ctrl-Shift-T".to_owned(),
                "Reopen the most recently closed tab".to_owned(),
            ),
            ("Esc".to_owned(), "Close the active overlay".to_owned()),
        ]
        .into_iter()
        .filter(|(key, description)| {
            query.is_empty()
                || key.to_lowercase().contains(&query)
                || description.to_lowercase().contains(&query)
        })
        .collect()
    }

    fn buffer(&self) -> &Buffer {
        &self.buffers[self.active_tab]
    }

    fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.active_tab]
    }

    fn active_git_root(&self) -> PathBuf {
        self.buffer()
            .path()
            .and_then(|path| {
                self.workspace_roots
                    .iter()
                    .find(|root| path.starts_with(root))
                    .cloned()
            })
            .unwrap_or_else(|| self.search_root.clone())
    }

    fn refresh_git_context(&mut self) {
        let root = self.active_git_root();
        self.branch = git_branch(&root);
        self.branches = git_branches(&root);
        self.branch_index = self
            .branch
            .as_ref()
            .and_then(|current| self.branches.iter().position(|branch| branch == current))
            .unwrap_or(0);
    }

    fn terminal_mut(&mut self) -> &mut TerminalState {
        &mut self.terminals[self.active_terminal]
    }

    fn write_terminal_cursor_key(&mut self, normal: &[u8], application: &[u8]) -> AppResult<()> {
        let use_application = self.terminals[self.active_terminal].application_cursor_keys();
        self.terminal_mut()
            .write_bytes(if use_application { application } else { normal })
    }

    fn write_agent_cursor_key(&mut self, normal: &[u8], application: &[u8]) -> AppResult<()> {
        let use_application = self.agent.application_cursor_keys();
        self.agent
            .write_bytes(if use_application { application } else { normal })
    }

    fn open_terminal(&mut self) {
        let command = terminal_command(&self.settings);
        let cwd = self.terminals[self.active_terminal]
            .working_directory_path()
            .filter(|path| path.is_dir());
        let mut terminal = TerminalState::with_command(command, cwd);
        terminal.set_scrollback(self.settings.terminal.scrollback);
        self.terminals.push(terminal);
        self.active_terminal = self.terminals.len() - 1;
        self.terminal_selection = None;
        self.terminal_selection_dragging = false;
        self.status_message = format!(
            "Terminal {}/{}",
            self.active_terminal + 1,
            self.terminals.len()
        );
    }

    fn switch_terminal(&mut self, delta: isize) {
        if self.terminals.len() < 2 {
            return;
        }
        self.active_terminal = (self.active_terminal as isize + delta)
            .rem_euclid(self.terminals.len() as isize) as usize;
        self.terminal_selection = None;
        self.terminal_selection_dragging = false;
        self.status_message = format!(
            "Terminal {}/{}",
            self.active_terminal + 1,
            self.terminals.len()
        );
    }

    pub fn tab_titles(&self) -> Vec<String> {
        self.buffers
            .iter()
            .map(|buffer| {
                if buffer.dirty() {
                    format!("{} •", buffer.file_name())
                } else {
                    buffer.file_name().to_owned()
                }
            })
            .collect()
    }

    pub const fn active_tab(&self) -> usize {
        self.active_tab
    }

    pub const fn horizontal_scroll(&self) -> usize {
        self.horizontal_scroll
    }
    pub const fn editor_scroll_line(&self) -> Option<usize> {
        self.editor_scroll_line
    }
    pub fn scroll_editor(&mut self, delta: isize) {
        let max_line = self.file_lines().len().saturating_sub(1);
        let current = self.editor_scroll_line.unwrap_or(0);
        self.editor_scroll_line = Some(current.saturating_add_signed(delta).min(max_line));
    }
    pub fn scroll_editor_horizontal(&mut self, delta: isize) {
        if self.word_wrap() {
            self.horizontal_scroll = 0;
            return;
        }
        let line_width = self
            .file_lines()
            .get(self.cursor().line)
            .map(|line| {
                line.chars().fold(0usize, |column, character| {
                    if character == '\t' {
                        let tab_size = self.settings.editor.tab_size.max(1);
                        column + (tab_size - column % tab_size)
                    } else {
                        column + UnicodeWidthChar::width(character).unwrap_or(0)
                    }
                })
            })
            .unwrap_or_default();
        let max_scroll = if self.editor_viewport_width.get() == 0 {
            usize::MAX
        } else {
            line_width.saturating_sub(self.editor_viewport_width.get())
        };
        self.horizontal_scroll = self
            .horizontal_scroll
            .saturating_add_signed(delta)
            .min(max_scroll);
    }
    pub fn set_editor_viewport_width(&self, width: usize) {
        self.editor_viewport_width.set(width);
    }
    pub fn drain_plugin_mouse_events(&mut self) -> Vec<MouseEvent> {
        std::mem::take(&mut self.plugin_mouse_events)
    }
    pub fn cursor_positions(&self) -> Vec<Position> {
        std::iter::once(self.cursor())
            .chain(self.secondary_cursors.iter().copied())
            .collect()
    }
    pub const fn column_selection_active(&self) -> bool {
        self.column_selection.is_some()
    }
    pub fn mouse_down(&mut self, position: Position, extend: bool) {
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.column_selection = None;
        if !extend {
            self.selection_anchor = Some(position);
        } else {
            self.selection_anchor.get_or_insert(position);
        }
        self.buffer_mut().set_cursor(position.line, position.column);
        self.mouse_selecting = true;
        self.focus = Focus::Editor;
        self.emit_plugin_event("cursor.change");
    }

    pub fn mouse_column_down(&mut self, position: Position) {
        self.selection_anchor = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.column_selection = Some((position, position));
        self.buffer_mut().set_cursor(position.line, position.column);
        self.mouse_selecting = true;
        self.focus = Focus::Editor;
        self.emit_plugin_event("cursor.change");
    }
    pub fn mouse_add_cursor(&mut self, position: Position) {
        let line = position.line.min(self.file_lines().len().saturating_sub(1));
        let column = position.column.min(self.file_lines()[line].chars().count());
        let candidate = Position { line, column };
        if candidate == self.cursor() {
            return;
        }
        if let Some(index) = self
            .secondary_cursors
            .iter()
            .position(|cursor| *cursor == candidate)
        {
            self.secondary_cursors.remove(index);
        } else {
            self.secondary_cursors.push(candidate);
            self.secondary_cursors
                .sort_by_key(|cursor| (cursor.line, cursor.column));
        }
        self.selection_anchor = None;
        self.secondary_selections.clear();
        self.column_selection = None;
        self.focus = Focus::Editor;
        self.status_message = format!("{} cursors", self.secondary_cursors.len() + 1);
        self.emit_plugin_event("cursor.change");
    }
    pub fn mouse_drag(&mut self, position: Position) {
        if self.mouse_selecting {
            self.buffer_mut().set_cursor(position.line, position.column);
            self.emit_plugin_event("cursor.change");
        }
    }

    pub fn mouse_column_drag(&mut self, position: Position) {
        if let Some((anchor, _)) = self.column_selection {
            self.column_selection = Some((anchor, position));
            self.buffer_mut().set_cursor(position.line, position.column);
            self.emit_plugin_event("cursor.change");
        }
    }

    pub fn mouse_up(&mut self) {
        self.mouse_selecting = false;
        let Some((anchor, end)) = self.column_selection.take() else {
            return;
        };
        let first_line = anchor.line.min(end.line);
        let last_line = anchor.line.max(end.line);
        let first_column = anchor.column.min(end.column);
        let last_column = anchor.column.max(end.column);
        if first_column == last_column {
            self.buffer_mut().set_cursor(end.line, end.column);
            self.emit_plugin_event("cursor.change");
            return;
        }
        let ranges: Vec<_> = (first_line..=last_line)
            .filter_map(|line| {
                let length = self.file_lines().get(line)?.chars().count();
                let start = first_column.min(length);
                let finish = last_column.min(length);
                (start < finish).then_some((
                    Position {
                        line,
                        column: start,
                    },
                    Position {
                        line,
                        column: finish,
                    },
                ))
            })
            .collect();
        let Some((primary_start, primary_end)) = ranges.first().copied() else {
            self.selection_anchor = None;
            return;
        };
        self.selection_anchor = Some(primary_start);
        self.buffer_mut()
            .set_cursor(primary_end.line, primary_end.column);
        self.secondary_selections = ranges.into_iter().skip(1).collect();
        self.status_message = format!("Column selection · {} lines", last_line - first_line + 1);
        self.emit_plugin_event("cursor.change");
    }
    pub const fn word_wrap(&self) -> bool {
        self.settings.editor.word_wrap
    }
    pub fn selection_style_inverted(&self) -> bool {
        self.settings
            .editor
            .selection_style
            .eq_ignore_ascii_case("invert")
    }
    pub fn word_wrap_column(&self) -> usize {
        self.settings.editor.word_wrap_column.max(1)
    }

    fn open_buffer(&mut self, buffer: Buffer) {
        let mut buffer = buffer;
        if let Some(path) = buffer.path().map(Path::to_path_buf) {
            if let Some(language) = self.language_overrides.get(&path_key(&path)) {
                buffer.set_language_override(Some(language.clone()));
            }
        }
        self.close_plugin_file_view();
        self.invalidate_lsp_requests();
        self.git_blame_generation.fetch_add(1, Ordering::AcqRel);
        self.blame_lines.clear();
        self.git_history_generation.fetch_add(1, Ordering::AcqRel);
        self.history.clear();
        self.history_snapshot_head = None;
        self.history_offset = 0;
        self.history_has_more = false;
        self.history_pending = false;
        self.completion_due = None;
        self.diff_base_lines = None;
        self.folded_lines.clear();
        if let Some(path) = buffer.path() {
            if let Some(index) = self
                .buffers
                .iter()
                .position(|candidate| candidate.path() == Some(path))
            {
                self.active_tab = index;
                self.selection_anchor = None;
                self.secondary_cursors.clear();
                self.secondary_selections.clear();
                self.folded_lines.clear();
                self.diff_base_lines = None;
                self.horizontal_scroll = 0;
                self.editor_scroll_line = None;
                self.refresh_search_results();
                self.refresh_git_context();
                self.emit_plugin_event("tab.activate");
                self.reveal_active_file();
                return;
            }
        }
        self.buffers.push(buffer);
        self.active_tab = self.buffers.len() - 1;
        self.selection_anchor = None;
        self.editor_scroll_line = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.refresh_search_results();
        self.refresh_git_context();
        self.emit_plugin_event("tab.open");
        self.emit_plugin_event("tab.activate");
        self.reveal_active_file();
    }

    pub fn open_external_target(&mut self, target: &str) -> AppResult<()> {
        let target = parse_location_target(target);
        if target.path.is_dir() {
            if self.add_workspace_folder(&target.path)? {
                self.status_message = format!("Added workspace folder: {}", target.path.display());
            } else {
                self.status_message =
                    format!("Workspace folder already open: {}", target.path.display());
            }
            return Ok(());
        }
        self.open_external_file_target(target)
    }

    fn add_workspace_folder(&mut self, path: &Path) -> AppResult<bool> {
        let path = path.canonicalize().map_err(|error| {
            crate::error::AppError::Workspace(format!("workspace folder: {error}"))
        })?;
        if !path.is_dir() {
            return Err(crate::error::AppError::Workspace(format!(
                "workspace folder is not a directory: {}",
                path.display()
            )));
        }
        if self.workspace_roots.iter().any(|root| root == &path) {
            return Ok(false);
        }
        self.workspace_roots.push(path.clone());
        self.explorer_roots.push(path);
        if let Some(root) = self.workspace_roots.last() {
            for runtime in &self.plugin_runtimes {
                runtime.add_workspace_root(root);
            }
        }
        self.refresh_git_state();
        self.refresh_explorer();
        self.refresh_search_results();
        self.refresh_quick_open_results();
        Ok(true)
    }

    fn save_workspace_file(&self, path: &Path) -> AppResult<()> {
        if path.exists() {
            return Err(crate::error::AppError::Workspace(format!(
                "workspace save refuses to overwrite existing path: {}",
                path.display()
            )));
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let base = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        let folders = self
            .workspace_roots
            .iter()
            .map(|root| WorkspaceFolder {
                path: root
                    .strip_prefix(&base)
                    .map_or_else(|_| root.clone(), PathBuf::from),
            })
            .collect();
        let contents = serde_json::to_string_pretty(&WorkspaceFile { folders })
            .map_err(|error| crate::error::AppError::Workspace(error.to_string()))?;
        fs::write(path, format!("{contents}\n"))?;
        Ok(())
    }

    fn remove_active_workspace_folder(&mut self) -> AppResult<bool> {
        if self.workspace_roots.len() <= 1 {
            self.status_message = "Cannot remove the last workspace folder".to_owned();
            return Ok(false);
        }
        let active_path = self.buffer().path().unwrap_or(&self.search_root);
        let Some(index) = self
            .workspace_roots
            .iter()
            .position(|root| active_path.starts_with(root))
        else {
            self.status_message = "Active file is outside workspace folders".to_owned();
            return Ok(false);
        };
        let removed = self.workspace_roots.remove(index);
        self.explorer_roots.retain(|root| root != &removed);
        for runtime in &self.plugin_runtimes {
            runtime.remove_workspace_root(&removed);
        }
        self.refresh_git_state();
        self.refresh_explorer();
        self.refresh_search_results();
        self.refresh_quick_open_results();
        self.status_message = format!("Removed workspace folder: {}", removed.display());
        Ok(true)
    }

    fn open_plugin_file(&mut self, path: &Path) -> AppResult<bool> {
        for (runtime_index, runtime) in self.plugin_runtimes.iter().enumerate() {
            let Some(text) = runtime.open_file(path)? else {
                continue;
            };
            let close_id = runtime.take_file_close_id();
            let plugin_title = runtime.take_file_title();
            self.close_plugin_file_view();
            let title = plugin_title.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Plugin file")
                    .to_owned()
            });
            self.plugin_file_view = Some((title, text));
            self.plugin_file_view_path = None;
            self.git_diff_tab_text = None;
            self.plugin_diff_file_path = None;
            self.plugin_file_close = close_id.map(|id| (runtime_index, id));
            self.focus = Focus::Editor;
            self.emit_plugin_event("tab.open");
            self.emit_plugin_event("tab.activate");
            self.status_message = format!("Opened plugin file {}", path.display());
            return Ok(true);
        }
        Ok(false)
    }

    fn close_plugin_file_view(&mut self) {
        self.plugin_view_hover_link = None;
        if let Some((runtime_index, id)) = self.plugin_custom_tab.take() {
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                if let Err(error) = runtime.close_tab(&id) {
                    self.status_message = format!("Plugin tab close failed: {error}");
                }
            }
            self.plugin_file_view = None;
            self.plugin_file_view_path = None;
            self.git_diff_tab_text = None;
            self.plugin_diff_file_path = None;
            self.plugin_tab_scroll = 0;
            self.plugin_tab_hunk = 0;
            self.plugin_tab_split = false;
            self.plugin_tab_collapsed_hunks.clear();
            self.emit_plugin_event("tab.close");
            self.emit_plugin_event("tab.activate");
            return;
        }
        let Some((runtime_index, close_id)) = self.plugin_file_close.take() else {
            self.plugin_file_view = None;
            self.plugin_file_view_path = None;
            self.git_diff_tab_text = None;
            self.plugin_diff_file_path = None;
            return;
        };
        if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
            if let Err(error) = runtime.close_file(close_id) {
                self.status_message = format!("Plugin tab close failed: {error}");
            }
        }
        self.emit_plugin_event("tab.close");
        self.emit_plugin_event("tab.activate");
        self.plugin_file_view = None;
        self.plugin_file_view_path = None;
        self.git_diff_tab_text = None;
        self.plugin_diff_file_path = None;
        self.plugin_tab_scroll = 0;
        self.plugin_tab_hunk = 0;
        self.plugin_tab_split = false;
        self.plugin_tab_collapsed_hunks.clear();
    }

    fn close_active_tab(&mut self) {
        if self.buffers.len() == 1 {
            self.status_message = "At least one editor tab must remain".to_owned();
            return;
        }
        if self.buffer().dirty() {
            self.status_message = "Tab has unsaved changes; save before closing".to_owned();
            return;
        }
        self.emit_plugin_event("document.close");
        if let Some(path) = self.buffer().path().map(Path::to_path_buf) {
            let language = self.language().to_owned();
            self.notify_lsp_closed(&language, &path);
        }
        self.emit_plugin_event("tab.close");
        self.invalidate_lsp_requests();
        self.git_blame_generation.fetch_add(1, Ordering::AcqRel);
        self.blame_lines.clear();
        self.git_history_generation.fetch_add(1, Ordering::AcqRel);
        self.history.clear();
        self.history_snapshot_head = None;
        self.history_offset = 0;
        self.history_has_more = false;
        self.history_pending = false;
        self.completion_due = None;
        let closed_tab = ClosedTab {
            buffer: self.buffers.remove(self.active_tab),
            selection_anchor: self.selection_anchor.take(),
            secondary_cursors: std::mem::take(&mut self.secondary_cursors),
            secondary_selections: std::mem::take(&mut self.secondary_selections),
            column_selection: self.column_selection.take(),
            horizontal_scroll: self.horizontal_scroll,
            editor_scroll_line: self.editor_scroll_line.take(),
            folded_lines: std::mem::take(&mut self.folded_lines),
            diff_base_lines: self.diff_base_lines.take(),
        };
        self.closed_tabs.push(closed_tab);
        const MAX_CLOSED_TABS: usize = 20;
        if self.closed_tabs.len() > MAX_CLOSED_TABS {
            self.closed_tabs.remove(0);
        }
        self.active_tab = self.active_tab.min(self.buffers.len() - 1);
        self.column_selection = None;
        self.horizontal_scroll = 0;
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.emit_plugin_event("tab.activate");
    }

    fn reopen_closed_tab(&mut self) {
        let Some(closed_tab) = self.closed_tabs.pop() else {
            self.status_message = "No closed editor tab to reopen".to_owned();
            return;
        };
        if let Some(path) = closed_tab.buffer.path() {
            if let Some(index) = self
                .buffers
                .iter()
                .position(|buffer| buffer.path() == Some(path))
            {
                self.active_tab = index;
                self.status_message = "Closed tab is already open".to_owned();
                return;
            }
        }
        self.close_plugin_file_view();
        self.invalidate_lsp_requests();
        self.buffers.push(closed_tab.buffer);
        self.active_tab = self.buffers.len() - 1;
        self.selection_anchor = closed_tab.selection_anchor;
        self.secondary_cursors = closed_tab.secondary_cursors;
        self.secondary_selections = closed_tab.secondary_selections;
        self.column_selection = closed_tab.column_selection;
        self.horizontal_scroll = closed_tab.horizontal_scroll;
        self.editor_scroll_line = closed_tab.editor_scroll_line;
        self.folded_lines = closed_tab.folded_lines;
        self.diff_base_lines = closed_tab.diff_base_lines;
        self.git_blame_generation.fetch_add(1, Ordering::AcqRel);
        self.blame_lines.clear();
        self.git_history_generation.fetch_add(1, Ordering::AcqRel);
        self.history.clear();
        self.history_snapshot_head = None;
        self.history_offset = 0;
        self.history_has_more = false;
        self.history_pending = false;
        self.completion_due = None;
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.refresh_git_context();
        self.emit_plugin_event("tab.open");
        self.emit_plugin_event("tab.activate");
        self.status_message = format!("Reopened {}", self.file_name());
    }

    fn switch_tab(&mut self, delta: isize) {
        if self.buffers.len() < 2 {
            return;
        }
        self.active_tab =
            (self.active_tab as isize + delta).rem_euclid(self.buffers.len() as isize) as usize;
        self.invalidate_lsp_requests();
        self.git_blame_generation.fetch_add(1, Ordering::AcqRel);
        self.blame_lines.clear();
        self.git_history_generation.fetch_add(1, Ordering::AcqRel);
        self.history.clear();
        self.history_snapshot_head = None;
        self.history_offset = 0;
        self.history_has_more = false;
        self.history_pending = false;
        self.completion_due = None;
        self.selection_anchor = None;
        self.horizontal_scroll = 0;
        self.editor_scroll_line = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.folded_lines.clear();
        self.diff_base_lines = None;
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.refresh_git_context();
        self.emit_plugin_event("tab.activate");
        self.status_message = format!("Tab {}/{}", self.active_tab + 1, self.buffers.len());
    }

    fn invalidate_lsp_requests(&mut self) {
        let pending: Vec<_> = self.lsp_pending.keys().cloned().collect();
        self.lsp_pending.clear();
        for (language, id) in pending {
            let _ = self.lsp.cancel_request(&language, id);
        }
        self.lsp_generation = self.lsp_generation.wrapping_add(1);
    }

    fn normalize_super_shortcut(&self, key: KeyEvent) -> KeyEvent {
        let modifier_key = self
            .settings
            .keyboard
            .modifier_key
            .trim()
            .to_ascii_lowercase();
        if matches!(modifier_key.as_str(), "control+super" | "super")
            && key.modifiers.contains(KeyModifiers::SUPER)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let modifiers = (key.modifiers & !KeyModifiers::SUPER) | KeyModifiers::CONTROL;
            KeyEvent::new(key.code, modifiers)
        } else {
            key
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AppResult<()> {
        self.editor_hover_link = None;
        self.plugin_view_hover_link = None;
        self.output_hover_link = None;
        let raw_key = key;
        // Super/Cmd normalization is a host shortcut concern. Interactive
        // terminal and Agent sessions must receive the user's real modifiers.
        let key = if matches!(self.focus, Focus::Terminal | Focus::Agent) {
            raw_key
        } else {
            self.normalize_super_shortcut(raw_key)
        };
        if self.git_credential_prompt.is_some() {
            self.handle_git_credential_key(key);
            return Ok(());
        }
        if self.focus == Focus::Editor
            && matches!(key.code, KeyCode::Char('t' | 'T'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            self.reopen_closed_tab();
            return Ok(());
        }
        if self.plugin_command_line.is_some() {
            self.handle_plugin_command_line_key(key)?;
            return Ok(());
        }
        if self.plugin_dialog.is_some() {
            if let Some(PluginDialog::Custom {
                runtime_index, id, ..
            }) = self.plugin_dialog.clone()
            {
                if key.code == KeyCode::Esc {
                    self.close_plugin_dialog(false)?;
                    return Ok(());
                }
                if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                    match key.code {
                        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            runtime.dialog_focus_previous(id);
                        }
                        KeyCode::Tab => {
                            runtime.dialog_focus_next(id);
                        }
                        KeyCode::Enter => {
                            if !runtime.dialog_input_key(id, "Enter", None, "")? {
                                runtime.dialog_activate(id)?;
                            }
                        }
                        KeyCode::Up => {
                            if !runtime.dialog_input_key(id, "Up", None, "")? {
                                runtime.dialog_dropdown_move(id, -1)?;
                            }
                        }
                        KeyCode::Left => {
                            if !runtime.dialog_input_key(id, "Left", None, "")? {
                                runtime.dialog_dropdown_move(id, -1)?;
                            }
                        }
                        KeyCode::Down => {
                            if !runtime.dialog_input_key(id, "Down", None, "")? {
                                runtime.dialog_dropdown_move(id, 1)?;
                            }
                        }
                        KeyCode::Right => {
                            if !runtime.dialog_input_key(id, "Right", None, "")? {
                                runtime.dialog_dropdown_move(id, 1)?;
                            }
                        }
                        KeyCode::Backspace => {
                            runtime.dialog_input_key(id, "Backspace", None, "")?;
                        }
                        KeyCode::Delete => {
                            runtime.dialog_input_key(id, "Delete", None, "")?;
                        }
                        KeyCode::Home => {
                            runtime.dialog_input_key(id, "Home", None, "")?;
                        }
                        KeyCode::End => {
                            runtime.dialog_input_key(id, "End", None, "")?;
                        }
                        KeyCode::Char(value) if key.modifiers.is_empty() => {
                            runtime.dialog_input_key(id, "Char", Some(value), "")?;
                        }
                        _ => {}
                    }
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Enter => self.close_plugin_dialog(true)?,
                KeyCode::Esc => self.close_plugin_dialog(false)?,
                _ => {}
            }
            return Ok(());
        }
        if self.focus == Focus::Search
            && key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('v')
        {
            let clipboard = self.clipboard.clone();
            self.handle_paste(&clipboard)?;
            return Ok(());
        }
        if self.handle_plugin_drawer_key(key)? {
            return Ok(());
        }
        if self.plugin_context_menu.is_some() {
            self.handle_plugin_context_menu_key(key)?;
            return Ok(());
        }
        if self.settings_overlay {
            self.handle_settings_key(key);
            return Ok(());
        }
        if self.plugin_palette {
            self.handle_plugin_palette_key(key)?;
            return Ok(());
        }
        if self.quick_open {
            self.handle_quick_open_key(key)?;
            return Ok(());
        }
        if self.goto_mode {
            self.handle_goto_key(key);
            return Ok(());
        }
        if self.focus == Focus::Editor {
            let key_name = editor_plugin_key_name(raw_key);
            let rune = match raw_key.code {
                KeyCode::Char(value) if !raw_key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(value)
                }
                _ => None,
            };
            let modifier = format_key_modifier(raw_key.modifiers);
            let snapshot = self.plugin_editor_snapshot();
            let diff_snapshot = self.plugin_diff_snapshot();
            for index in 0..self.plugin_runtimes.len() {
                let consumed = {
                    let runtime = &self.plugin_runtimes[index];
                    runtime.set_editor_snapshot(snapshot.clone());
                    runtime.set_diff_snapshot(diff_snapshot.clone());
                    runtime.emit_key_event(&key_name, rune, &modifier)?
                };
                if consumed {
                    self.apply_plugin_editor_requests();
                    self.apply_plugin_bookmark_events();
                    let snapshot = self.plugin_editor_snapshot();
                    let diff_snapshot = self.plugin_diff_snapshot();
                    let runtime = &self.plugin_runtimes[index];
                    runtime.set_editor_snapshot(snapshot);
                    runtime.set_diff_snapshot(diff_snapshot);
                    return Ok(());
                }
            }
        }
        if let Some(consumed) = self.handle_key_chord(key) {
            if consumed {
                return Ok(());
            }
        }
        if self.sidebar_tab == SidebarTab::Search
            && key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('r')
            && self.focus != Focus::Search
        {
            self.replace_mode = true;
            self.focus = Focus::Search;
            self.search_field = 0;
            return Ok(());
        }
        if self.sidebar_tab == SidebarTab::Search
            && self.replace_mode
            && key.code == KeyCode::Esc
            && self.focus != Focus::Search
        {
            self.replace_mode = false;
            self.focus = Focus::Editor;
            return Ok(());
        }
        if self.focus == Focus::Editor
            && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('t')
        {
            return self.handle_editor_key(key);
        }
        if self.dispatch_plugin_keybinding(key)? {
            return Ok(());
        }
        if self.dispatch_plugin_panel_key(key)? {
            return Ok(());
        }
        if matches!(
            self.focus,
            Focus::Explorer
                | Focus::Search
                | Focus::Changes
                | Focus::Branches
                | Focus::Output
                | Focus::Agent
        ) && key.modifiers == (KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            match key.code {
                KeyCode::Up => {
                    self.resize_bottom_panel(1);
                    return Ok(());
                }
                KeyCode::Down => {
                    self.resize_bottom_panel(-1);
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.focus == Focus::Search {
            self.handle_search_key(key);
            return Ok(());
        }
        if self.focus == Focus::Explorer {
            self.handle_explorer_key(key)?;
            return Ok(());
        }
        if self.focus == Focus::Terminal {
            self.handle_terminal_key(key)?;
            return Ok(());
        }
        if self.focus == Focus::Changes {
            self.handle_changes_key(key)?;
            return Ok(());
        }
        if self.focus == Focus::Output {
            self.handle_output_key(key);
            return Ok(());
        }
        if self.focus == Focus::Branches {
            self.handle_branches_key(key)?;
            return Ok(());
        }
        if self.focus == Focus::Agent {
            self.handle_agent_key(key)?;
            return Ok(());
        }
        if self.focus == Focus::Editor {
            return self.handle_editor_key(key);
        }
        self.handle_navigation(key);
        Ok(())
    }

    fn handle_key_chord(&mut self, key: KeyEvent) -> Option<bool> {
        if self.key_chord_prefix {
            self.key_chord_prefix = false;
            if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Char('p') => {
                        self.open_quick_open();
                        return Some(true);
                    }
                    KeyCode::Char('e') => {
                        self.sidebar_visible = true;
                        self.select_sidebar(SidebarTab::Explorer);
                        return Some(true);
                    }
                    KeyCode::Char('f') => {
                        self.sidebar_visible = true;
                        self.select_sidebar(SidebarTab::Search);
                        return Some(true);
                    }
                    KeyCode::Char('c') => {
                        self.sidebar_visible = true;
                        self.select_sidebar(SidebarTab::Changes);
                        return Some(true);
                    }
                    KeyCode::Char('d') => {
                        self.sidebar_visible = true;
                        self.select_sidebar(SidebarTab::Problems);
                        return Some(true);
                    }
                    KeyCode::Char('t') => {
                        self.bottom_visible = true;
                        self.bottom_tab = BottomTab::Terminal;
                        self.open_terminal();
                        self.focus = Focus::Terminal;
                        return Some(true);
                    }
                    _ => {}
                }
            }
            return None;
        }
        if self.focus != Focus::Terminal
            && self.focus != Focus::Agent
            && key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('k')
        {
            self.key_chord_prefix = true;
            self.status_message =
                "Ctrl-K chord · P Open · E Explorer · F Find · C Changes · D Problems · T Terminal"
                    .to_owned();
            return Some(true);
        }
        None
    }

    fn handle_git_credential_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => {
                if let Some(prompt) = self.git_credential_prompt.as_mut() {
                    prompt.password_focus = !prompt.password_focus;
                }
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if let Some(prompt) = self.git_credential_prompt.as_mut() {
                    if prompt.password_focus {
                        let _ = edit_overlay_text(
                            &mut prompt.password,
                            &mut prompt.password_cursor,
                            "Backspace",
                            None,
                        );
                    } else {
                        let _ = edit_overlay_text(
                            &mut prompt.username,
                            &mut prompt.username_cursor,
                            "Backspace",
                            None,
                        );
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Delete)
            | (KeyModifiers::NONE, KeyCode::Left)
            | (KeyModifiers::NONE, KeyCode::Right)
            | (KeyModifiers::NONE, KeyCode::Home)
            | (KeyModifiers::NONE, KeyCode::End) => {
                if let Some(prompt) = self.git_credential_prompt.as_mut() {
                    let key_name = match key.code {
                        KeyCode::Delete => "Delete",
                        KeyCode::Left => "Left",
                        KeyCode::Right => "Right",
                        KeyCode::Home => "Home",
                        KeyCode::End => "End",
                        _ => unreachable!(),
                    };
                    if prompt.password_focus {
                        let _ = edit_overlay_text(
                            &mut prompt.password,
                            &mut prompt.password_cursor,
                            key_name,
                            None,
                        );
                    } else {
                        let _ = edit_overlay_text(
                            &mut prompt.username,
                            &mut prompt.username_cursor,
                            key_name,
                            None,
                        );
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                if let Some(prompt) = self.git_credential_prompt.as_mut() {
                    if prompt.password_focus {
                        let _ = edit_overlay_text(
                            &mut prompt.password,
                            &mut prompt.password_cursor,
                            "Char",
                            Some(character),
                        );
                    } else {
                        let _ = edit_overlay_text(
                            &mut prompt.username,
                            &mut prompt.username_cursor,
                            "Char",
                            Some(character),
                        );
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.git_credential_prompt = None;
                self.status_message = "Git authentication cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let Some(prompt) = self.git_credential_prompt.clone() else {
                    return;
                };
                if self.git_remote_pending.is_some() {
                    return;
                }
                let root = prompt.root.clone();
                let sender = self.git_remote_tx.clone();
                let operation = prompt.operation.clone();
                let username = prompt.username.clone();
                let password = prompt.password.clone();
                let worker_operation = operation.clone();
                if std::thread::Builder::new()
                    .name("mtide-git-remote-auth".to_owned())
                    .spawn(move || {
                        let result =
                            git_remote_credentials(&root, &worker_operation, &username, &password)
                                .map_err(|error| error.to_string());
                        let _ = sender.send(GitRemoteCompletion {
                            operation: worker_operation,
                            root,
                            result,
                        });
                    })
                    .is_err()
                {
                    self.status_message = "Unable to start Git authentication retry".to_owned();
                    return;
                }
                self.git_credential_prompt = None;
                self.git_remote_pending = Some(operation.clone());
                self.status_message = format!("Git {operation} retry running…");
            }
            _ => {}
        }
    }
    fn open_quick_open(&mut self) {
        self.begin_overlay();
        self.quick_open = true;
        self.quick_open_save_as = false;
        self.quick_open_save_workspace = false;
        self.quick_open_add_folder = false;
        self.quick_open_query.clear();
        self.quick_open_cursor = 0;
        self.refresh_quick_open_results();
        self.status_message = "Open file · Enter open · Esc cancel".to_owned();
    }
    fn open_save_as(&mut self) {
        self.begin_overlay();
        self.quick_open = true;
        self.quick_open_save_as = true;
        self.quick_open_save_workspace = false;
        self.quick_open_add_folder = false;
        self.quick_open_query = self.buffer().file_name().to_owned();
        self.quick_open_cursor = self.quick_open_query.chars().count();
        self.quick_open_results.clear();
        self.quick_open_index = 0;
        self.status_message = "Save As · Enter save · Esc cancel".to_owned();
    }
    fn open_save_workspace(&mut self) {
        self.begin_overlay();
        self.quick_open = true;
        self.quick_open_save_as = false;
        self.quick_open_save_workspace = true;
        self.quick_open_add_folder = false;
        self.quick_open_query.clear();
        self.quick_open_cursor = 0;
        self.quick_open_results.clear();
        self.quick_open_index = 0;
        self.status_message = "Save Workspace · Enter save · Esc cancel".to_owned();
    }
    fn open_add_workspace_folder(&mut self) {
        self.begin_overlay();
        self.quick_open = true;
        self.quick_open_save_as = false;
        self.quick_open_save_workspace = false;
        self.quick_open_add_folder = true;
        self.quick_open_query.clear();
        self.quick_open_cursor = 0;
        self.quick_open_results.clear();
        self.quick_open_index = 0;
        self.status_message = "Add Workspace Folder · Enter add · Esc cancel".to_owned();
    }
    fn open_goto(&mut self) {
        self.begin_overlay();
        self.goto_mode = true;
        self.goto_input.clear();
        self.goto_cursor = 0;
        self.status_message = "Go to line[:column] · Enter jump · Esc cancel".to_owned();
    }
    fn close_goto(&mut self) {
        self.goto_mode = false;
        self.goto_input.clear();
        self.goto_cursor = 0;
        self.restore_overlay_focus();
    }
    fn handle_goto_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character))
                if character.is_ascii_digit() || character == ':' =>
            {
                let _ = edit_overlay_text(
                    &mut self.goto_input,
                    &mut self.goto_cursor,
                    "Char",
                    Some(character),
                );
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                let _ = edit_overlay_text(
                    &mut self.goto_input,
                    &mut self.goto_cursor,
                    "Backspace",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                let _ =
                    edit_overlay_text(&mut self.goto_input, &mut self.goto_cursor, "Delete", None);
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                let _ =
                    edit_overlay_text(&mut self.goto_input, &mut self.goto_cursor, "Left", None);
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                let _ =
                    edit_overlay_text(&mut self.goto_input, &mut self.goto_cursor, "Right", None);
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                let _ =
                    edit_overlay_text(&mut self.goto_input, &mut self.goto_cursor, "Home", None);
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                let _ = edit_overlay_text(&mut self.goto_input, &mut self.goto_cursor, "End", None);
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.close_goto();
                self.status_message = "Go to line cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let mut parts = self.goto_input.split(':');
                let Some(line) = parts.next().and_then(|value| value.parse::<usize>().ok()) else {
                    self.status_message = "Enter a line number".to_owned();
                    return;
                };
                let column = parts
                    .next()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1);
                let line = line.saturating_sub(1);
                let column = column.saturating_sub(1);
                self.buffer_mut().set_cursor(line, column);
                self.close_goto();
                self.status_message = format!("Moved to {}:{}", line + 1, column + 1);
            }
            _ => {}
        }
    }
    fn close_quick_open(&mut self) {
        self.quick_open = false;
        self.quick_open_query.clear();
        self.quick_open_save_as = false;
        self.quick_open_save_workspace = false;
        self.quick_open_add_folder = false;
        self.quick_open_results.clear();
        self.quick_open_cursor = 0;
        self.quick_open_index = 0;
        self.restore_overlay_focus();
    }
    fn refresh_quick_open_results(&mut self) {
        if self.quick_open_save_as
            || self.quick_open_save_workspace
            || self.quick_open_add_folder
            || self.quick_open_query.starts_with('>')
            || self.quick_open_query.starts_with('?')
        {
            self.quick_open_results.clear();
            self.quick_open_index = 0;
            return;
        }
        let query = self.quick_open_query.trim().to_ascii_lowercase();
        let mut candidates = Vec::new();
        for root in &self.workspace_roots {
            collect_quick_open_files(
                root,
                root,
                self.settings.quick_open.show_hidden,
                self.settings.quick_open.show_git_ignored,
                &mut candidates,
            );
        }
        candidates.retain(|path| {
            query.is_empty()
                || quick_open_match_score(
                    &quick_open_path_label(path, &self.workspace_roots),
                    &query,
                ) < usize::MAX
        });
        candidates.sort_by_key(|path| {
            let display = quick_open_path_label(path, &self.workspace_roots);
            (quick_open_match_score(&display, &query), display)
        });
        candidates.truncate(100);
        self.quick_open_results = candidates;
        self.quick_open_index = self
            .quick_open_index
            .min(self.quick_open_results.len().saturating_sub(1));
    }
    fn handle_quick_open_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Char",
                    Some(character),
                );
                self.refresh_quick_open_results();
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Backspace",
                    None,
                );
                self.refresh_quick_open_results();
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Delete",
                    None,
                );
                self.refresh_quick_open_results();
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Left",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Right",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Home",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "End",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.close_quick_open();
                self.status_message = "Open file cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.quick_open_query.starts_with('>') {
                    let items = self.command_palette_items(&self.quick_open_query);
                    if let Some((id, _)) = items.get(self.quick_open_index) {
                        let id = id.clone();
                        self.close_quick_open();
                        if self.execute_builtin_command(&id)? {
                            self.status_message = format!("Command executed · {id}");
                        } else if self.execute_plugin_command(&id)? {
                            self.status_message = format!("Plugin command executed · {id}");
                        }
                    } else {
                        self.status_message = "No matching command".to_owned();
                    }
                    return Ok(());
                }
                if self.quick_open_query.starts_with('?') {
                    self.close_quick_open();
                    self.status_message = "Keyboard help closed".to_owned();
                    return Ok(());
                }
                let raw = self.quick_open_query.trim();
                if raw.is_empty() {
                    self.status_message = "Enter a file path".to_owned();
                    return Ok(());
                }
                let path = self
                    .quick_open_results
                    .get(self.quick_open_index)
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(raw));
                let path = if path.is_absolute() {
                    path
                } else {
                    self.search_root.join(path)
                };
                if self.quick_open_add_folder {
                    if !path.is_dir() {
                        self.status_message =
                            format!("Workspace folder not found: {}", path.display());
                        return Ok(());
                    }
                    if self.add_workspace_folder(&path)? {
                        self.close_quick_open();
                        self.status_message = format!("Added workspace folder: {}", path.display());
                    } else {
                        self.status_message = "Workspace folder already open".to_owned();
                    }
                    return Ok(());
                }
                if self.quick_open_save_workspace {
                    let mut workspace_path = path;
                    if workspace_path.extension().is_none() {
                        workspace_path.set_extension("ttt");
                    }
                    self.save_workspace_file(&workspace_path)?;
                    self.close_quick_open();
                    self.status_message =
                        format!("Saved workspace as {}", workspace_path.display());
                    return Ok(());
                }
                if self.quick_open_save_as {
                    if self.buffer().has_unresolved_conflicts() {
                        self.status_message =
                            "Save As blocked: resolve merge conflicts first".to_owned();
                        return Ok(());
                    }
                    if path.exists() {
                        self.status_message =
                            "Save As refuses to overwrite an existing path".to_owned();
                        return Ok(());
                    }
                    let editor = self.settings.editor.clone();
                    self.buffer_mut().save_as(
                        &path,
                        editor.trim_trailing_whitespace,
                        editor.insert_final_newline,
                    )?;
                    self.sync_lsp_for_buffer();
                    let language = self.language().to_owned();
                    self.notify_lsp_saved(&language, &path);
                    self.emit_plugin_event("document.save");
                    self.refresh_git_state();
                    self.overlay_focus = None;
                    self.focus = Focus::Editor;
                    self.close_quick_open();
                    self.status_message = format!("Saved as {}", path.display());
                    return Ok(());
                }
                if !path.is_file() {
                    self.status_message = format!("File not found: {}", path.display());
                    return Ok(());
                }
                if !self.open_plugin_file(&path)? {
                    self.open_buffer(Buffer::open(&path)?);
                }
                self.sync_lsp_for_buffer();
                self.emit_plugin_event("document.open");
                self.overlay_focus = None;
                self.focus = Focus::Editor;
                self.close_quick_open();
                self.status_message = format!("Opened {}", path.display());
            }
            (KeyModifiers::NONE, KeyCode::Down)
                if !self.quick_open_save_as
                    && !self.quick_open_save_workspace
                    && !self.quick_open_add_folder
                    && self.quick_open_query.starts_with('>') =>
            {
                self.quick_open_index = (self.quick_open_index + 1).min(
                    self.command_palette_items(&self.quick_open_query)
                        .len()
                        .saturating_sub(1),
                );
            }
            (KeyModifiers::NONE, KeyCode::Down)
                if !self.quick_open_save_as
                    && !self.quick_open_save_workspace
                    && !self.quick_open_add_folder
                    && self.quick_open_query.starts_with('?') =>
            {
                self.quick_open_index = (self.quick_open_index + 1)
                    .min(self.quick_open_help_items().len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Down)
                if !self.quick_open_save_as
                    && !self.quick_open_save_workspace
                    && !self.quick_open_add_folder =>
            {
                self.quick_open_index = (self.quick_open_index + 1)
                    .min(self.quick_open_results.len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Up)
                if !self.quick_open_save_as
                    && !self.quick_open_save_workspace
                    && !self.quick_open_add_folder
                    && self.quick_open_query.starts_with('>') =>
            {
                self.quick_open_index = self.quick_open_index.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Up)
                if !self.quick_open_save_as
                    && !self.quick_open_save_workspace
                    && !self.quick_open_add_folder
                    && self.quick_open_query.starts_with('?') =>
            {
                self.quick_open_index = self.quick_open_index.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Up)
                if !self.quick_open_save_as
                    && !self.quick_open_save_workspace
                    && !self.quick_open_add_folder =>
            {
                self.quick_open_index = self.quick_open_index.saturating_sub(1);
            }
            _ => {}
        }
        Ok(())
    }
    pub fn handle_paste(&mut self, text: &str) -> AppResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        if let Some(prompt) = self.git_credential_prompt.as_mut() {
            if prompt.password_focus {
                for character in text.chars() {
                    let _ = edit_overlay_text(
                        &mut prompt.password,
                        &mut prompt.password_cursor,
                        "Char",
                        Some(character),
                    );
                }
            } else {
                for character in text.chars() {
                    let _ = edit_overlay_text(
                        &mut prompt.username,
                        &mut prompt.username_cursor,
                        "Char",
                        Some(character),
                    );
                }
            }
            return Ok(());
        }
        if self.commit_mode || self.pull_request_comment_mode {
            for character in text.chars() {
                let _ = edit_overlay_text(
                    &mut self.commit_message,
                    &mut self.commit_cursor,
                    "Char",
                    Some(character),
                );
            }
            return Ok(());
        }
        if self.branch_mode {
            for character in text.chars() {
                let _ = edit_overlay_text(
                    &mut self.branch_name,
                    &mut self.branch_cursor,
                    "Char",
                    Some(character),
                );
            }
            return Ok(());
        }
        if self.explorer_mode.as_ref().is_some_and(|mode| {
            matches!(
                mode,
                ExplorerMode::CreateFile | ExplorerMode::CreateDirectory | ExplorerMode::Rename(_)
            )
        }) {
            for character in text.chars() {
                let _ = edit_overlay_text(
                    &mut self.explorer_input,
                    &mut self.explorer_cursor,
                    "Char",
                    Some(character),
                );
            }
            return Ok(());
        }
        if let Some(command_line) = self.plugin_command_line.as_ref() {
            let runtime_index = command_line.runtime_index;
            let next_text = {
                let command_line = self.plugin_command_line.as_mut().expect("checked above");
                for character in text.chars() {
                    let _ = edit_command_line(command_line, "Char", Some(character));
                }
                command_line.text.clone()
            };
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                runtime.command_line_change(&next_text)?;
            }
            return Ok(());
        }
        if let Some(PluginDialog::Custom {
            runtime_index, id, ..
        }) = self.plugin_dialog.clone()
        {
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                for character in text.chars() {
                    let _ = runtime.dialog_append(id, character)?;
                }
            }
            return Ok(());
        }
        if self.quick_open {
            for character in text.chars() {
                let _ = edit_overlay_text(
                    &mut self.quick_open_query,
                    &mut self.quick_open_cursor,
                    "Char",
                    Some(character),
                );
            }
            self.refresh_quick_open_results();
            return Ok(());
        }
        if self.goto_mode {
            for character in text.chars() {
                if character.is_ascii_digit() || character == ':' {
                    let _ = edit_overlay_text(
                        &mut self.goto_input,
                        &mut self.goto_cursor,
                        "Char",
                        Some(character),
                    );
                }
            }
            return Ok(());
        }
        if self.focus == Focus::Search {
            for character in text.chars() {
                self.edit_search_input("Char", Some(character));
            }
            self.request_search_refresh();
            return Ok(());
        }
        if self.plugin_palette {
            self.plugin_query.push_str(text);
            self.plugin_index = 0;
            return Ok(());
        }
        if self.paste_to_plugin_input(text)? {
            return Ok(());
        }
        match self.focus {
            Focus::Search => unreachable!("search paste returns before focus routing"),
            Focus::Editor => {
                if !self.secondary_selections.is_empty() {
                    self.replace_active_selections(text);
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                if let Some(anchor) = self.selection_anchor.take() {
                    let cursor = self.buffer().cursor();
                    self.buffer_mut().delete_between(anchor, cursor);
                }
                if self.secondary_cursors.is_empty() {
                    self.buffer_mut().insert(text);
                } else {
                    self.apply_multi_cursor_insert(text);
                }
                self.sync_lsp_for_buffer();
                self.refresh_search_results();
                self.emit_plugin_event("editor.change");
            }
            Focus::Terminal => {
                if self.terminal_bracketed_paste() {
                    let mut payload = Vec::with_capacity(text.len() + 12);
                    payload.extend_from_slice(b"\x1b[200~");
                    payload.extend_from_slice(text.as_bytes());
                    payload.extend_from_slice(b"\x1b[201~");
                    self.terminal_mut().write_bytes(&payload)?;
                } else {
                    self.terminal_mut().append_input(text);
                }
            }
            Focus::Agent => {
                if self.agent.bracketed_paste() {
                    let mut payload = Vec::with_capacity(text.len() + 12);
                    payload.extend_from_slice(b"\x1b[200~");
                    payload.extend_from_slice(text.as_bytes());
                    payload.extend_from_slice(b"\x1b[201~");
                    self.agent.write_bytes(&payload)?;
                } else {
                    self.agent.append_input(text);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn paste_to_plugin_input(&mut self, text: &str) -> AppResult<bool> {
        let mut targets = Vec::new();
        if let Some((runtime_index, id)) = self.plugin_custom_tab.clone() {
            if self.focus == Focus::Editor {
                targets.push((runtime_index, format!("__tab:{id}")));
            }
        }
        if targets.is_empty() {
            let location = match self.focus {
                Focus::Explorer => Some(PluginPanelLocation::Sidebar),
                Focus::Output => Some(PluginPanelLocation::Bottom),
                _ => None,
            };
            if let Some(location) = location {
                targets.extend(self.plugin_runtimes.iter().enumerate().filter_map(
                    |(runtime_index, runtime)| {
                        runtime
                            .registration()
                            .panels
                            .into_iter()
                            .find(|panel| panel.location == location)
                            .map(|panel| (runtime_index, panel.id))
                    },
                ));
            }
        }
        for (runtime_index, panel_id) in targets {
            let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
                continue;
            };
            let mut handled = false;
            for character in text.chars() {
                let (key_name, rune) = if character == '\n' {
                    ("Enter", None)
                } else {
                    ("Char", Some(character))
                };
                let accepted = runtime.panel_input_key(&panel_id, key_name, rune, "")?
                    || runtime.panel_password_key(&panel_id, key_name, rune, "")?;
                if !accepted {
                    break;
                }
                handled = true;
            }
            if handled {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
        }
        if let Some(drawer) = self.plugin_drawer.clone() {
            let Some(runtime) = self.plugin_runtimes.get(drawer.runtime_index) else {
                return Ok(false);
            };
            let mut handled = false;
            for character in text.chars() {
                let (key_name, rune) = if character == '\n' {
                    ("Enter", None)
                } else {
                    ("Char", Some(character))
                };
                if !runtime.drawer_input_key(key_name, rune, "")?
                    && !runtime.drawer_password_key(key_name, rune, "")?
                {
                    break;
                }
                handled = true;
            }
            if handled {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn handle_plugin_palette_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                self.plugin_query.push(character);
                self.plugin_index = 0;
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.plugin_query.pop();
                self.plugin_index = 0;
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.plugin_index = self.plugin_index.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.plugin_index = (self.plugin_index + 1)
                    .min(self.plugin_palette_items().len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Esc) => self.close_plugin_palette(),
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some((id, _)) = self.plugin_palette_items().get(self.plugin_index) {
                    let id = id.clone();
                    self.close_plugin_palette();
                    if self.execute_builtin_command(&id)? {
                        self.status_message = format!("Command executed · {id}");
                    } else if self.execute_plugin_command(&id)? {
                        self.status_message = format!("Plugin command executed · {id}");
                    }
                    return Ok(());
                }
                self.close_plugin_palette();
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_settings_key(&mut self, key: KeyEvent) {
        let count = self.settings_rows().len();
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.settings_index = self.settings_index.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.settings_index = (self.settings_index + 1).min(count.saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Left) if self.settings_index == 10 => {
                self.cycle_indent_size(-1);
            }
            (KeyModifiers::NONE, KeyCode::Right) if self.settings_index == 10 => {
                self.cycle_indent_size(1);
            }
            (KeyModifiers::NONE, KeyCode::Left | KeyCode::Right) if self.settings_index == 16 => {
                self.cycle_keyboard_modifier()
            }
            (KeyModifiers::NONE, KeyCode::Left | KeyCode::Right) if self.settings_index == 18 => {
                self.cycle_auto_save_delay(if key.code == KeyCode::Left { -1 } else { 1 });
            }
            (KeyModifiers::NONE, KeyCode::Esc) => self.close_settings(),
            (KeyModifiers::CONTROL, KeyCode::Char('s')) => match self.settings.save() {
                Ok(()) => self.status_message = "Settings saved".to_owned(),
                Err(error) => self.status_message = format!("Settings save failed: {error}"),
            },
            (KeyModifiers::NONE, KeyCode::Enter | KeyCode::Char(' ')) => {
                self.toggle_setting(self.settings_index);
            }
            _ => {}
        }
    }
    fn toggle_setting(&mut self, index: usize) {
        match index {
            0 => self.sidebar_visible = !self.sidebar_visible,
            1 => self.bottom_visible = !self.bottom_visible,
            2 => self.agent_visible = !self.agent_visible,
            3 => {
                self.settings.editor.word_wrap = !self.settings.editor.word_wrap;
                if self.settings.editor.word_wrap {
                    self.horizontal_scroll = 0;
                }
            }
            4 => self.settings.editor.format_on_save = !self.settings.editor.format_on_save,
            5 => self.settings.save_all_on_quit = !self.settings.save_all_on_quit,
            6 => self.settings.terminal.ctrl_v = !self.settings.terminal.ctrl_v,
            7 => self.settings.lsp.enabled = !self.settings.lsp.enabled,
            8 => {
                self.auto_detect_indent = !self.auto_detect_indent;
                self.settings.editor.auto_detect_indent = self.auto_detect_indent;
            }
            9 => {
                let use_tabs = !self.effective_use_tabs();
                self.auto_detect_indent = false;
                self.settings.editor.auto_detect_indent = false;
                self.settings.editor.use_tabs = use_tabs;
            }
            10 => self.cycle_indent_size(1),
            11 => {
                self.settings.quick_open.show_hidden = !self.settings.quick_open.show_hidden;
                self.refresh_quick_open_results();
            }
            12 => {
                self.settings.quick_open.show_git_ignored =
                    !self.settings.quick_open.show_git_ignored;
                self.refresh_quick_open_results();
            }
            13 => {
                self.settings.explorer.show_hidden = !self.settings.explorer.show_hidden;
                self.refresh_explorer();
            }
            14 => {
                self.settings.explorer.show_git_ignored = !self.settings.explorer.show_git_ignored;
                self.refresh_explorer();
            }
            15 => {
                self.settings.explorer.auto_reveal = !self.settings.explorer.auto_reveal;
                self.reveal_active_file();
            }
            16 => self.cycle_keyboard_modifier(),
            17 => self.settings.editor.auto_save = !self.settings.editor.auto_save,
            18 => self.cycle_auto_save_delay(1),
            19 => {
                self.settings.editor.selection_style = if self
                    .settings
                    .editor
                    .selection_style
                    .eq_ignore_ascii_case("invert")
                {
                    "background"
                } else {
                    "invert"
                }
                .to_owned();
            }
            20 => self.settings.editor.diff_word_wrap = !self.settings.editor.diff_word_wrap,
            21 => {
                self.settings.editor.diff_high_contrast = !self.settings.editor.diff_high_contrast
            }
            _ => return,
        }
        self.status_message = "Setting changed for this session".to_owned();
    }
    fn cycle_keyboard_modifier(&mut self) {
        const MODIFIERS: [&str; 3] = ["control", "control+super", "super"];
        let current = self.settings.keyboard.modifier_key.to_ascii_lowercase();
        let index = MODIFIERS
            .iter()
            .position(|mode| *mode == current)
            .unwrap_or(0);
        self.settings.keyboard.modifier_key = MODIFIERS[(index + 1) % MODIFIERS.len()].to_owned();
    }
    fn cycle_auto_save_delay(&mut self, delta: isize) {
        const DELAYS: [u64; 4] = [1_000, 5_000, 10_000, 30_000];
        let current = self.settings.editor.auto_save_delay_ms;
        let index = DELAYS
            .iter()
            .position(|delay| *delay == current)
            .unwrap_or(1);
        let next = (index as isize + delta).rem_euclid(DELAYS.len() as isize) as usize;
        self.settings.editor.auto_save_delay_ms = DELAYS[next];
    }
    fn cycle_indent_size(&mut self, delta: isize) {
        const SIZES: [usize; 3] = [2, 4, 8];
        let current = self.effective_indent_size();
        let index = SIZES.iter().position(|size| *size == current).unwrap_or(1);
        let next = (index as isize + delta).rem_euclid(SIZES.len() as isize) as usize;
        self.settings.editor.indent_size = SIZES[next];
        self.auto_detect_indent = false;
        self.settings.editor.auto_detect_indent = false;
    }
    fn open_settings(&mut self) {
        self.begin_overlay();
        self.settings_overlay = true;
        self.settings_index = self
            .settings_index
            .min(self.settings_rows().len().saturating_sub(1));
    }
    fn close_settings(&mut self) {
        self.settings_overlay = false;
        self.restore_overlay_focus();
    }
    fn open_plugin_palette(&mut self) {
        self.begin_overlay();
        self.plugin_palette = true;
        self.plugin_query.clear();
        self.plugin_index = 0;
    }
    fn close_plugin_palette(&mut self) {
        self.plugin_palette = false;
        self.plugin_query.clear();
        self.plugin_index = 0;
        self.restore_overlay_focus();
    }
    fn begin_overlay(&mut self) {
        self.overlay_focus.get_or_insert(self.focus);
    }
    fn restore_overlay_focus(&mut self) {
        if let Some(focus) = self.overlay_focus.take() {
            self.focus = focus;
        }
    }
    fn execute_plugin_command(&mut self, id: &str) -> AppResult<bool> {
        for runtime in &self.plugin_runtimes {
            if runtime.execute_command(id)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                self.apply_plugin_panel_requests();
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn execute_builtin_command(&mut self, id: &str) -> AppResult<bool> {
        match id {
            "options.toggleDiffWordWrap" => {
                self.settings.editor.diff_word_wrap = !self.settings.editor.diff_word_wrap;
                self.persist_settings()?;
                self.status_message = format!(
                    "Diff word wrap {}",
                    if self.settings.editor.diff_word_wrap {
                        "on"
                    } else {
                        "off"
                    }
                );
                return Ok(true);
            }
            "options.toggleDiffHighContrast" => {
                self.settings.editor.diff_high_contrast = !self.settings.editor.diff_high_contrast;
                self.persist_settings()?;
                self.status_message = format!(
                    "Diff high contrast {}",
                    if self.settings.editor.diff_high_contrast {
                        "on"
                    } else {
                        "off"
                    }
                );
                return Ok(true);
            }
            "options.useChangesOnlyDiff" | "options.useFullFileDiff" => {
                self.diff_full_context = id == "options.useFullFileDiff";
                self.settings.editor.diff_context = if self.diff_full_context {
                    "full"
                } else {
                    "changes"
                }
                .to_owned();
                self.persist_settings()?;
                self.reload_commit_detail_context();
                self.status_message = if self.diff_full_context {
                    "Diff context: full file"
                } else {
                    "Diff context: changes only"
                }
                .to_owned();
                return Ok(true);
            }
            "options.useSplitDiff" | "options.useUnifiedDiff" => {
                self.diff_split = id == "options.useSplitDiff";
                self.status_message = if self.diff_split {
                    "Diff view: split"
                } else {
                    "Diff view: unified"
                }
                .to_owned();
                return Ok(true);
            }
            "editor.scrollLeft" | "editor.scrollRight" => {
                let delta = if id == "editor.scrollLeft" { -4 } else { 4 };
                self.scroll_editor_horizontal(delta);
                self.status_message = if self.word_wrap() {
                    "Horizontal scrolling requires word wrap to be off".to_owned()
                } else if delta < 0 {
                    "Editor scrolled left".to_owned()
                } else {
                    "Editor scrolled right".to_owned()
                };
                return Ok(true);
            }
            _ => {}
        }
        if id == "plugin.refreshRegistry" {
            let endpoint = std::env::var("MTIDE_PLUGIN_REGISTRY").map_err(|_| {
                crate::error::AppError::Plugin(
                    "MTIDE_PLUGIN_REGISTRY is required for registry refresh".into(),
                )
            })?;
            self.plugin_registry_entries = fetch_plugin_registry(&endpoint)?;
            self.bottom_visible = true;
            self.bottom_tab = BottomTab::Output;
            self.focus = Focus::Output;
            self.status_message = format!(
                "Loaded {} plugin registry entries",
                self.plugin_registry_entries.len()
            );
            return Ok(true);
        }
        if id == "editor.formatExternal" {
            self.run_external_formatter()?;
            return Ok(true);
        }
        let key = match id {
            "lsp.restart" => {
                self.restart_lsp()?;
                return Ok(true);
            }
            "lsp.workspaceSymbols" => {
                self.request_lsp("workspace/symbol")?;
                return Ok(true);
            }
            "editor.save" => KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            "editor.reopenClosedTab" => KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            "editor.undo" => KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            "editor.redo" => KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            "editor.find" => KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            "editor.gotoLine" => KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            "lsp.documentSymbols" => KeyEvent::new(
                KeyCode::Char('o'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            "editor.goToMatchingBracket" => KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            "editor.joinLines" => KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            "editor.sortLines" => {
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT | KeyModifiers::SHIFT)
            }
            "editor.uppercase" => {
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT | KeyModifiers::SHIFT)
            }
            "editor.lowercase" => {
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT | KeyModifiers::SHIFT)
            }
            "editor.splitSelectionLines" => {
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT | KeyModifiers::SHIFT)
            }
            "editor.duplicateLine" => KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            "editor.deleteLine" => KeyEvent::new(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            "editor.deleteWordLeft" | "editor.deleteWordBackward" => {
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)
            }
            "editor.deleteWordRight" | "editor.deleteWordForward" => {
                KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)
            }
            "editor.toggleComment" => KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
            "view.toggleSidebar" => KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            "view.toggleTerminal" => KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            "view.toggleAgent" => KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            "view.switchTheme" => KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            "workspace.openFile" => KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            "git.sync" => KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            "workspace.saveAs" => {
                self.open_save_workspace();
                return Ok(true);
            }
            "workspace.addFolder" => {
                self.open_add_workspace_folder();
                return Ok(true);
            }
            "workspace.removeFolder" => {
                self.remove_active_workspace_folder()?;
                return Ok(true);
            }
            "settings.open" => {
                self.open_settings();
                return Ok(true);
            }
            _ => return Ok(false),
        };
        self.handle_key(key)?;
        Ok(true)
    }
    fn dispatch_plugin_keybinding(&mut self, key: KeyEvent) -> AppResult<bool> {
        let Some(chord) = key_chord(key) else {
            return Ok(false);
        };
        for runtime in &self.plugin_runtimes {
            if runtime.trigger_keybinding(&chord)? {
                self.apply_plugin_editor_requests();
                self.apply_plugin_bookmark_events();
                self.status_message = format!("Plugin command executed · {chord}");
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn emit_plugin_event(&mut self, event: &str) {
        let snapshot = self.plugin_editor_snapshot();
        let cursor = self.buffer().cursor();
        let buffer = self.buffer();
        let path = snapshot.path.clone();
        let payload = match event {
            "document.open" | "document.reload" | "document.save" | "document.close" => {
                vec![PluginEventArg::Text(path.clone())]
            }
            "tab.activate" | "tab.open" | "tab.close" => {
                vec![
                    PluginEventArg::Text(path.clone()),
                    PluginEventArg::Number(cursor.line + 1),
                    PluginEventArg::Number(cursor.column + 1),
                ]
            }
            "editor.change" | "cursor.change" => vec![
                PluginEventArg::Text(path.clone()),
                PluginEventArg::Number(cursor.line + 1),
                PluginEventArg::Number(cursor.column + 1),
                PluginEventArg::Text(snapshot.language.clone()),
                PluginEventArg::Bool(snapshot.selection.is_some()),
                PluginEventArg::Text(snapshot.selection_text.clone()),
                PluginEventArg::Text(buffer.lines().get(cursor.line).cloned().unwrap_or_default()),
                PluginEventArg::Number(buffer.lines().len()),
                PluginEventArg::Bool(buffer.dirty()),
                // These fields are appended to preserve the original TTT positional API.
                PluginEventArg::Number(
                    snapshot
                        .selection
                        .map(|selection| selection.0 + 1)
                        .unwrap_or(0),
                ),
                PluginEventArg::Number(
                    snapshot
                        .selection
                        .map(|selection| selection.1 + 1)
                        .unwrap_or(0),
                ),
                PluginEventArg::Number(
                    snapshot
                        .selection
                        .map(|selection| selection.2 + 1)
                        .unwrap_or(0),
                ),
                PluginEventArg::Number(
                    snapshot
                        .selection
                        .map(|selection| selection.3 + 1)
                        .unwrap_or(0),
                ),
                PluginEventArg::Text(current_word_at_cursor(
                    buffer
                        .lines()
                        .get(cursor.line)
                        .map(String::as_str)
                        .unwrap_or_default(),
                    cursor.column,
                )),
            ],
            _ => Vec::new(),
        };
        let event_names: Vec<&str> = match event {
            "document.open" => vec!["document.open", "file.open", "tab.change"],
            "document.reload" => vec!["document.reload", "file.open"],
            "document.save" => vec!["document.save", "file.save"],
            "document.close" => vec!["document.close", "file.close"],
            "tab.activate" => vec!["tab.activate", "tab.change"],
            _ => vec![event],
        };
        for runtime in &self.plugin_runtimes {
            runtime.set_editor_snapshot(snapshot.clone());
            runtime.set_diff_snapshot(self.plugin_diff_snapshot());
            for event_name in &event_names {
                if let Err(error) = runtime.emit_with_payload(event_name, &payload) {
                    self.status_message = format!("Plugin event failed: {error}");
                }
            }
        }
        self.apply_plugin_editor_requests();
        self.apply_plugin_bookmark_events();
    }

    fn plugin_editor_snapshot(&self) -> PluginEditorSnapshot {
        let buffer = self.buffer();
        let cursor = buffer.cursor();
        let selection = self
            .selection_anchor
            .map(|anchor| (anchor.line, anchor.column, cursor.line, cursor.column));
        let selection_text = self
            .selection_anchor
            .map(|anchor| buffer.text_between(anchor, cursor))
            .unwrap_or_default();
        PluginEditorSnapshot {
            lines: buffer.lines().to_vec(),
            path: buffer
                .path()
                .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
            file_name: buffer.file_name().to_owned(),
            language: self.language().to_owned(),
            cursor_line: cursor.line,
            cursor_col: cursor.column,
            cursors: self
                .cursor_positions()
                .into_iter()
                .map(|position| (position.line, position.column))
                .collect(),
            selection,
            selection_text,
            viewport_top_line: self.editor_scroll_line.unwrap_or(0),
            viewport_left_col: self.horizontal_scroll,
            viewport_height: self.editor_viewport_height.get(),
        }
    }

    pub fn set_editor_viewport_height(&self, height: usize) {
        self.editor_viewport_height.set(height);
    }

    pub fn emit_plugin_gutter_click(&mut self, line: usize) -> AppResult<bool> {
        let snapshot = self.plugin_editor_snapshot();
        let payload = [
            PluginEventArg::Text(snapshot.path.clone()),
            PluginEventArg::Number(line + 1),
        ];
        let mut handled = false;
        for runtime in &self.plugin_runtimes {
            runtime.set_editor_snapshot(snapshot.clone());
            runtime.set_diff_snapshot(self.plugin_diff_snapshot());
            match runtime.emit_with_payload("gutter.click", &payload) {
                Ok(consumed) => handled |= consumed,
                Err(error) => self.status_message = format!("Plugin gutter event failed: {error}"),
            }
        }
        self.apply_plugin_editor_requests();
        self.apply_plugin_bookmark_events();
        Ok(handled)
    }

    fn plugin_diff_snapshot(&self) -> Option<PluginDiffSnapshot> {
        let base = self.diff_base_lines.as_ref()?;
        let current = self.buffer().lines();
        let path = self
            .buffer()
            .path()
            .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let hunks = diff_change_blocks(base, current, path);
        let cursor_line = self.buffer().cursor().line;
        let current_hunk = hunks
            .iter()
            .rposition(|hunk| hunk.line <= cursor_line)
            .map_or(0, |index| index + 1);
        Some(PluginDiffSnapshot {
            hunks,
            current_hunk,
        })
    }

    fn apply_plugin_bookmark_events(&mut self) {
        let events = self
            .plugin_runtimes
            .iter()
            .flat_map(PluginRuntime::drain_bookmark_events)
            .collect::<Vec<_>>();
        for event in events {
            for runtime in &self.plugin_runtimes {
                if let Err(error) = runtime.emit_with_payload(
                    "bookmark.change",
                    &[
                        PluginEventArg::Text(event.path.clone()),
                        PluginEventArg::Number(event.line),
                        PluginEventArg::Text(event.action.clone()),
                        PluginEventArg::Text(event.icon.clone()),
                        PluginEventArg::Text(event.style.clone()),
                    ],
                ) {
                    self.status_message = format!("Plugin bookmark event failed: {error}");
                }
            }
        }
    }

    fn apply_plugin_automation_requests(&mut self) {
        let tasks = self
            .plugin_runtimes
            .iter()
            .enumerate()
            .flat_map(|(runtime_index, runtime)| {
                runtime
                    .drain_automation_requests()
                    .into_iter()
                    .map(move |task| (runtime_index, task))
            })
            .collect::<Vec<_>>();
        for (runtime_index, task) in tasks {
            let mut result = JsonValue::Null;
            let mut error = None;
            match task.action.as_str() {
                "session" => {
                    result = json!({
                        "pid": std::process::id(),
                        "name": "mtide",
                        "version": env!("CARGO_PKG_VERSION"),
                        "workspaces": self.workspace_roots.iter().map(|path| path.to_string_lossy().to_string()).collect::<Vec<_>>(),
                        "active_file": self.buffer().path().map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                    });
                }
                "buffer" => {
                    let cursor = self.cursor();
                    result = json!({
                        "text": self.file_lines().join("\n"),
                        "path": self.buffer().path().map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                        "cursor": {"line": cursor.line + 1, "col": cursor.column + 1},
                    });
                }
                "type" | "paste" => {
                    if let Err(app_error) = self.handle_paste(&task.args) {
                        error = Some(app_error.to_string());
                    } else {
                        result = json!(true);
                    }
                }
                "key" => match automation_key_events(&task.args) {
                    Ok(keys) => {
                        for key in keys {
                            if let Err(app_error) = self.handle_key(key) {
                                error = Some(app_error.to_string());
                                break;
                            }
                        }
                        if error.is_none() {
                            result = json!(true);
                        }
                    }
                    Err(message) => error = Some(message),
                },
                "click" | "rclick" | "right_click" | "hover" => {
                    match automation_mouse_events(&task.action, &task.args) {
                        Ok(events) => {
                            self.plugin_mouse_events.extend(events);
                            result = json!(true);
                        }
                        Err(message) => error = Some(message),
                    }
                }
                "drag" => match automation_mouse_coordinates(&task.args, 4) {
                    Ok([start_x, start_y, end_x, end_y]) => {
                        self.plugin_mouse_events.push(MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: start_x,
                            row: start_y,
                            modifiers: KeyModifiers::NONE,
                        });
                        self.plugin_mouse_events.push(MouseEvent {
                            kind: MouseEventKind::Drag(MouseButton::Left),
                            column: end_x,
                            row: end_y,
                            modifiers: KeyModifiers::NONE,
                        });
                        self.plugin_mouse_events.push(MouseEvent {
                            kind: MouseEventKind::Up(MouseButton::Left),
                            column: end_x,
                            row: end_y,
                            modifiers: KeyModifiers::NONE,
                        });
                        result = json!(true);
                    }
                    Err(message) => error = Some(message),
                },
                "scroll" => match automation_scroll_coordinates(&task.args) {
                    Ok((x, y, amount)) => {
                        let kind = if amount.is_positive() {
                            MouseEventKind::ScrollDown
                        } else {
                            MouseEventKind::ScrollUp
                        };
                        for _ in 0..amount.unsigned_abs() {
                            self.plugin_mouse_events.push(MouseEvent {
                                kind,
                                column: x,
                                row: y,
                                modifiers: KeyModifiers::NONE,
                            });
                        }
                        result = json!(true);
                    }
                    Err(message) => error = Some(message),
                },
                "open" => match serde_json::from_str::<JsonValue>(&task.args) {
                    Ok(params) => {
                        let path = params
                            .get("path")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if path.is_empty() {
                            error = Some("open requires path".to_owned());
                        } else {
                            let line = params
                                .get("line")
                                .and_then(JsonValue::as_u64)
                                .map(|line| line as usize);
                            let readonly = params
                                .get("readonly")
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false);
                            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                                runtime.enqueue_panel_request(PluginPanelRequest::OpenFile {
                                    path,
                                    line,
                                    readonly,
                                });
                            }
                            self.apply_plugin_panel_requests();
                            result = json!(true);
                        }
                    }
                    Err(parse_error) => error = Some(format!("invalid open params: {parse_error}")),
                },
                "screenshot" => {
                    result = json!(self.file_lines().join("\n"));
                }
                "debug" => {
                    result = json!({
                        "focus": format!("{:?}", self.focus),
                        "active_tab": self.active_tab(),
                        "cursor": {"line": self.cursor().line + 1, "col": self.cursor().column + 1},
                        "buffer_lines": self.file_lines().len(),
                    });
                }
                "wait-for" => match automation_wait_for(&task.args) {
                    Ok((needle, timeout)) => {
                        if self.automation_screenshot().contains(&needle) {
                            result = json!({"matched": true});
                        } else if timeout.is_zero() {
                            result = json!({"matched": false, "timed_out": true});
                        } else {
                            self.pending_automation_waits.push(PendingAutomationWait {
                                runtime_index,
                                id: task.id,
                                needle,
                                deadline: Instant::now() + timeout,
                            });
                            continue;
                        }
                    }
                    Err(message) => error = Some(message),
                },
                "quit" => {
                    self.quit();
                    result = json!({"ok": true});
                }
                action => error = Some(format!("unsupported automation action: {action}")),
            }
            if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                if let Err(app_error) = runtime.complete_automation(task.id, result, error) {
                    self.status_message = format!("Plugin automation callback failed: {app_error}");
                }
            }
        }
        self.poll_pending_automation_waits();
    }

    fn poll_pending_automation_waits(&mut self) {
        let screen = self.automation_screenshot();
        let now = Instant::now();
        let mut remaining = Vec::new();
        for wait in std::mem::take(&mut self.pending_automation_waits) {
            let Some(runtime) = self.plugin_runtimes.get(wait.runtime_index) else {
                continue;
            };
            if runtime.take_cancelled_automation(wait.id) {
                continue;
            }
            if screen.contains(&wait.needle) {
                if let Err(error) =
                    runtime.complete_automation(wait.id, json!({"matched": true}), None)
                {
                    self.status_message = format!("Plugin automation callback failed: {error}");
                }
            } else if now >= wait.deadline {
                if let Err(error) = runtime.complete_automation(
                    wait.id,
                    json!({"matched": false, "timed_out": true}),
                    Some("wait-for timeout".to_owned()),
                ) {
                    self.status_message = format!("Plugin automation callback failed: {error}");
                }
            } else {
                remaining.push(wait);
            }
        }
        self.pending_automation_waits = remaining;
    }

    fn apply_plugin_editor_requests(&mut self) {
        let requests = self
            .plugin_runtimes
            .iter()
            .flat_map(PluginRuntime::drain_editor_requests)
            .collect::<Vec<_>>();
        if requests.is_empty() {
            return;
        }
        let initial_cursor = self.cursor();
        let mut document_changed = false;
        let mut cursor_or_selection_changed = false;
        for request in requests {
            let line_count = self.file_lines().len();
            let clamp_position = |line: usize, col: usize, lines: &[String]| {
                let line = line.min(lines.len().saturating_sub(1));
                let col = col.min(lines.get(line).map_or(0, |text| text.chars().count()));
                Position { line, column: col }
            };
            match request {
                PluginEditorRequest::Undo => {
                    let before = self.file_lines().to_vec();
                    self.buffer_mut().undo();
                    document_changed |= self.file_lines() != before;
                }
                PluginEditorRequest::Redo => {
                    let before = self.file_lines().to_vec();
                    self.buffer_mut().redo();
                    document_changed |= self.file_lines() != before;
                }
                PluginEditorRequest::BeginUndoGroup => self.buffer_mut().begin_undo_group(),
                PluginEditorRequest::EndUndoGroup => self.buffer_mut().end_undo_group(),
                PluginEditorRequest::Insert { line, col, text } => {
                    let position = clamp_position(line, col, self.file_lines());
                    self.buffer_mut().set_cursor(position.line, position.column);
                    self.buffer_mut().insert(&text);
                    document_changed = true;
                }
                PluginEditorRequest::SetLine { line, text } => {
                    if line < line_count {
                        let end = self.file_lines()[line].chars().count();
                        self.buffer_mut().replace_ranges(
                            &[(Position { line, column: 0 }, Position { line, column: end })],
                            &text,
                        );
                        document_changed = true;
                    }
                }
                PluginEditorRequest::Replace {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                    text,
                } => {
                    let start = clamp_position(start_line, start_col, self.file_lines());
                    let end = clamp_position(end_line, end_col, self.file_lines());
                    self.buffer_mut().replace_ranges(&[(start, end)], &text);
                    document_changed = true;
                }
                PluginEditorRequest::SetCursor { line, col } => {
                    let position = clamp_position(line, col, self.file_lines());
                    self.buffer_mut().set_cursor(position.line, position.column);
                    self.selection_anchor = None;
                    cursor_or_selection_changed = true;
                }
                PluginEditorRequest::SetSelection {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                } => {
                    let start = clamp_position(start_line, start_col, self.file_lines());
                    let end = clamp_position(end_line, end_col, self.file_lines());
                    self.selection_anchor = Some(start);
                    self.buffer_mut().set_cursor(end.line, end.column);
                    cursor_or_selection_changed = true;
                }
                PluginEditorRequest::ClearSelection => {
                    self.selection_anchor = None;
                    self.secondary_selections.clear();
                    self.column_selection = None;
                    cursor_or_selection_changed = true;
                }
                PluginEditorRequest::AddCursor { line, col } => {
                    let position = clamp_position(line, col, self.file_lines());
                    if position != self.cursor() && !self.secondary_cursors.contains(&position) {
                        self.secondary_cursors.push(position);
                        self.secondary_cursors
                            .sort_by_key(|cursor| (cursor.line, cursor.column));
                        cursor_or_selection_changed = true;
                    }
                }
                PluginEditorRequest::ClearCursors => {
                    self.secondary_cursors.clear();
                    self.secondary_selections.clear();
                    self.column_selection = None;
                    cursor_or_selection_changed = true;
                }
                PluginEditorRequest::ScrollTo { line } => {
                    self.editor_scroll_line = Some(line.min(line_count.saturating_sub(1)));
                }
                PluginEditorRequest::ScrollBy { delta } => {
                    let current = self.editor_scroll_line.unwrap_or(0);
                    let next = if delta.is_negative() {
                        current.saturating_sub(delta.unsigned_abs())
                    } else {
                        current.saturating_add(delta as usize)
                    };
                    self.editor_scroll_line = Some(next.min(line_count.saturating_sub(1)));
                }
                PluginEditorRequest::SetSearch { pattern, use_regex } => {
                    self.search_query = pattern;
                    self.search_regex = use_regex;
                    self.replace_mode = false;
                    self.focus = Focus::Search;
                    self.sidebar_tab = SidebarTab::Search;
                    self.refresh_search();
                }
                PluginEditorRequest::ClearSearch => {
                    self.search_query.clear();
                    self.search_regex = false;
                    self.search_results.clear();
                    self.search_index = 0;
                }
                PluginEditorRequest::GoToDiffHunk { index } => {
                    if let Some(snapshot) = self.plugin_diff_snapshot() {
                        if let Some(hunk) = snapshot.hunks.get(index) {
                            let line = hunk.line.min(line_count.saturating_sub(1));
                            self.buffer_mut().set_cursor(line, 0);
                            self.editor_scroll_line = Some(line);
                            cursor_or_selection_changed = true;
                            self.status_message = format!(
                                "Diff hunk {}/{} · {}",
                                index + 1,
                                snapshot.hunks.len(),
                                hunk.kind
                            );
                        }
                    }
                }
            }
        }
        if document_changed {
            self.sync_lsp_for_buffer();
            self.refresh_search_results();
            self.emit_plugin_event("editor.change");
        } else if cursor_or_selection_changed || self.cursor() != initial_cursor {
            self.emit_plugin_event("cursor.change");
        }
    }
    fn selected_changes_tree_row(&self) -> Option<ChangeTreeRow> {
        let rows = self.changes_tree_rows();
        if let Some(selection) = &self.changes_tree_selection {
            rows.iter().find(|row| &row.key == selection).cloned()
        } else {
            rows.iter()
                .find(|row| row.file_index == Some(self.change_index))
                .cloned()
        }
    }

    fn select_changes_tree_row(&mut self, index: usize) {
        let rows = self.changes_tree_rows();
        let Some(row) = rows.get(index).cloned() else {
            return;
        };
        self.changes_tree_selection = Some(row.key);
        if let Some(change_index) = row.file_index {
            if self.change_index != change_index {
                self.change_index = change_index;
                self.refresh_change_diff();
            }
        }
        if index < self.changes_tree_scroll {
            self.changes_tree_scroll = index;
        } else if index >= self.changes_tree_scroll.saturating_add(12) {
            self.changes_tree_scroll = index.saturating_sub(11);
        }
    }

    fn move_changes_tree_selection(&mut self, delta: isize) {
        let rows = self.changes_tree_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .changes_tree_selection
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key == key))
            .or_else(|| {
                rows.iter()
                    .position(|row| row.file_index == Some(self.change_index))
            })
            .unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(rows.len().saturating_sub(1))
        };
        self.select_changes_tree_row(next);
    }

    fn toggle_changes_tree_directory(&mut self, row: &ChangeTreeRow, expand: Option<bool>) {
        if row.kind != ChangeTreeRowKind::Directory {
            return;
        }
        let is_collapsed = self.changes_tree_collapsed.contains(&row.key);
        let should_expand = expand.unwrap_or(is_collapsed);
        if should_expand {
            self.changes_tree_collapsed.remove(&row.key);
        } else {
            self.changes_tree_collapsed.insert(row.key.clone());
        }
        self.changes_tree_selection = Some(row.key.clone());
        let row_count = self.changes_tree_rows().len();
        self.changes_tree_scroll = self.changes_tree_scroll.min(row_count.saturating_sub(12));
    }

    fn changes_tree_parent_directory(&self, row: &ChangeTreeRow) -> Option<ChangeTreeRow> {
        let group = row.group?;
        if row.kind == ChangeTreeRowKind::Section {
            return None;
        }
        let rows = self.changes_tree_rows();
        let parent_directory = row.path.rsplit_once('/').and_then(|(parent, _)| {
            rows.iter().find(|candidate| {
                candidate.group == Some(group)
                    && candidate.kind == ChangeTreeRowKind::Directory
                    && candidate.path == parent
            })
        });
        parent_directory.cloned().or_else(|| {
            rows.into_iter().find(|candidate| {
                candidate.kind == ChangeTreeRowKind::Section && candidate.group == Some(group)
            })
        })
    }

    fn stage_changes_tree_directory(
        &mut self,
        row: &ChangeTreeRow,
        should_stage: bool,
    ) -> AppResult<()> {
        if self.git_state_pending {
            self.status_message = "Git refresh is still running".to_owned();
            return Ok(());
        }
        let prefix = row.path.as_str();
        let group = row.group;
        let selected: Vec<(PathBuf, String)> = self
            .changes
            .iter()
            .enumerate()
            .filter(|(index, change)| {
                let in_group = group.is_none_or(|group| match group {
                    ChangeGroup::Staged => change.index != ' ' && change.index != '?',
                    ChangeGroup::Changes => change.worktree != ' ' || change.index == '?',
                });
                let label = self.change_label(*index);
                in_group && (prefix.is_empty() || label.starts_with(&format!("{prefix}/")))
            })
            .filter(|(_, change)| {
                if should_stage {
                    change.worktree != ' ' || change.index == '?'
                } else {
                    change.index != ' ' && change.index != '?'
                }
            })
            .map(|(index, change)| (self.change_root(index).to_path_buf(), change.path.clone()))
            .collect();
        for (root, path) in &selected {
            if should_stage {
                git_stage(root, path)?;
            } else {
                git_unstage(root, path)?;
            }
        }
        if !selected.is_empty() {
            self.refresh_git_state();
        }
        self.status_message = format!(
            "{} {} file(s){}{}",
            if should_stage { "Staged" } else { "Unstaged" },
            selected.len(),
            if prefix.is_empty() { " in " } else { " under " },
            if prefix.is_empty() {
                row.group.map_or("workspace", ChangeGroup::label)
            } else {
                prefix
            }
        );
        Ok(())
    }

    fn handle_changes_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.commit_mode {
            return self.handle_commit_key(key);
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                self.settings.git.file_view = if self.settings.git.file_view == "tree" {
                    "list".to_owned()
                } else {
                    "tree".to_owned()
                };
                self.changes_tree_scroll = 0;
                self.status_message = match self.persist_settings() {
                    Ok(()) => format!("Git file view: {}", self.settings.git.file_view),
                    Err(error) => format!(
                        "Git file view changed to {}, settings save failed: {error}",
                        self.settings.git.file_view
                    ),
                };
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.move_changes_tree_selection(1);
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.move_changes_tree_selection(-1);
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(row) = self
                    .selected_changes_tree_row()
                    .filter(|row| row.kind != ChangeTreeRowKind::File)
                {
                    if row.kind == ChangeTreeRowKind::Section {
                        self.toggle_changes_tree_section(&row);
                    } else {
                        self.toggle_changes_tree_directory(&row, None);
                    }
                    return Ok(());
                }
            }
            (KeyModifiers::NONE, KeyCode::Left | KeyCode::Right) => {
                if let Some(row) = self.selected_changes_tree_row() {
                    if row.kind != ChangeTreeRowKind::File {
                        if key.code == KeyCode::Left && row.expanded {
                            if row.kind == ChangeTreeRowKind::Section {
                                self.toggle_changes_tree_section(&row);
                            } else {
                                self.toggle_changes_tree_directory(&row, Some(false));
                            }
                        } else if key.code == KeyCode::Right && !row.expanded {
                            if row.kind == ChangeTreeRowKind::Section {
                                self.toggle_changes_tree_section(&row);
                            } else {
                                self.toggle_changes_tree_directory(&row, Some(true));
                            }
                        } else if key.code == KeyCode::Right {
                            let rows = self.changes_tree_rows();
                            if let Some(index) =
                                rows.iter().position(|candidate| candidate.key == row.key)
                            {
                                if rows
                                    .get(index + 1)
                                    .is_some_and(|child| child.depth > row.depth)
                                {
                                    self.select_changes_tree_row(index + 1);
                                }
                            }
                        } else if let Some(parent) = self.changes_tree_parent_directory(&row) {
                            let index = self
                                .changes_tree_rows()
                                .iter()
                                .position(|candidate| candidate.key == parent.key)
                                .unwrap_or(0);
                            self.select_changes_tree_row(index);
                        }
                    } else if key.code == KeyCode::Left {
                        if let Some(parent) = self.changes_tree_parent_directory(&row) {
                            if parent.kind == ChangeTreeRowKind::Section {
                                self.toggle_changes_tree_section(&parent);
                            } else {
                                self.toggle_changes_tree_directory(&parent, Some(false));
                            }
                        }
                    }
                    return Ok(());
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('s' | 'u' | ' ')) => {
                if let Some(row) = self
                    .selected_changes_tree_row()
                    .filter(|row| row.kind != ChangeTreeRowKind::File)
                {
                    let should_stage = match key.code {
                        KeyCode::Char('u') => false,
                        KeyCode::Char(' ') => self
                            .changes_in_scope(&row)
                            .iter()
                            .any(|(_, change)| change.worktree != ' ' || change.index == '?'),
                        _ => true,
                    };
                    self.stage_changes_tree_directory(&row, should_stage)?;
                    return Ok(());
                }
            }
            _ => {}
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Down | KeyCode::Up) => unreachable!(),
            (KeyModifiers::NONE, KeyCode::Char('[')) => {
                self.change_hunk_index = self.change_hunk_index.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Char(']')) => {
                let count = git_hunk_count(&self.change_diff);
                if count > 0 {
                    self.change_hunk_index = (self.change_hunk_index + 1).min(count - 1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(change) = self.changes.get(self.change_index).cloned() {
                    let root = self.change_root(self.change_index).to_path_buf();
                    let path = root.join(&change.path);
                    if path.is_file() {
                        let base_lines = (change.index != '?' && change.worktree != '?')
                            .then(|| git_base_file(&root, &change.path))
                            .flatten();
                        let mut buffer = Buffer::open(&path)?;
                        if let Some(line) =
                            hunk_start_line(&self.change_diff, self.change_hunk_index)
                        {
                            buffer.set_cursor(line.saturating_sub(1), 0);
                        }
                        self.open_buffer(buffer);
                        self.diff_base_lines = base_lines;
                        self.sync_lsp_for_buffer();
                        self.emit_plugin_event("document.open");
                        self.focus = Focus::Editor;
                        self.status_message = format!("Opened {}", path.display());
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                self.bottom_visible = true;
                self.bottom_tab = BottomTab::Output;
                self.output_scroll = 0;
                self.focus = Focus::Output;
                self.status_message =
                    "Git diff · v split/unified · Up/Down/PageUp/PageDown/Home/End · Esc editor"
                        .to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('t')) if !self.change_diff.is_empty() => {
                self.open_git_diff_tab();
            }
            (KeyModifiers::NONE, KeyCode::Char('s')) => self.change_stage(true)?,
            (KeyModifiers::NONE, KeyCode::Char('u')) => self.change_stage(false)?,
            (KeyModifiers::NONE, KeyCode::Char(' ')) => {
                if let Some(change) = self.changes.get(self.change_index).cloned() {
                    let should_stage = change.index == ' ' || change.worktree != ' ';
                    self.change_stage(should_stage)?;
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('a')) => self.change_stage_all(true)?,
            (KeyModifiers::SHIFT, KeyCode::Char('A')) => self.change_stage_all(false)?,
            (KeyModifiers::NONE, KeyCode::Char('h')) => self.change_hunk_stage(true)?,
            (KeyModifiers::SHIFT, KeyCode::Char('H')) => self.change_hunk_stage(false)?,
            (KeyModifiers::NONE, KeyCode::Char('o' | 't')) => {
                self.resolve_selected_conflict(key.code == KeyCode::Char('o'))?
            }
            (KeyModifiers::NONE, KeyCode::Char('c')) => {
                self.commit_mode = true;
                self.commit_message.clear();
                self.commit_cursor = 0;
                self.status_message = "Commit message · Enter commit · Esc cancel".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('b')) => self.load_blame(),
            (KeyModifiers::NONE, KeyCode::Char('l')) => self.load_history(),
            (KeyModifiers::NONE, KeyCode::Char('f')) => self.run_git_remote("fetch"),
            (KeyModifiers::NONE, KeyCode::Char('p')) => self.run_git_remote("pull"),
            (KeyModifiers::SHIFT, KeyCode::Char('P')) => self.run_git_remote("push"),
            (KeyModifiers::SHIFT, KeyCode::Char('Y')) => self.run_git_remote("sync"),
            _ => self.handle_navigation(key),
        }
        Ok(())
    }

    fn toggle_changes_tree_section(&mut self, row: &ChangeTreeRow) {
        if row.kind != ChangeTreeRowKind::Section {
            return;
        }
        if row.expanded {
            self.changes_tree_collapsed.insert(row.key.clone());
        } else {
            self.changes_tree_collapsed.remove(&row.key);
        }
        self.changes_tree_selection = Some(row.key.clone());
        let row_count = self.changes_tree_rows().len();
        self.changes_tree_scroll = self.changes_tree_scroll.min(row_count.saturating_sub(12));
    }

    fn changes_in_scope(&self, row: &ChangeTreeRow) -> Vec<(usize, &GitChange)> {
        self.changes
            .iter()
            .enumerate()
            .filter(|(index, change)| {
                let in_group = row.group.is_none_or(|group| match group {
                    ChangeGroup::Staged => change.index != ' ' && change.index != '?',
                    ChangeGroup::Changes => change.worktree != ' ' || change.index == '?',
                });
                let label = self.change_label(*index);
                in_group && (row.path.is_empty() || label.starts_with(&format!("{}/", row.path)))
            })
            .collect()
    }

    fn persist_settings(&self) -> AppResult<()> {
        if let Some(path) = self.settings_path.as_deref() {
            self.settings.save_to_path(path)
        } else {
            self.settings.save()
        }
    }

    fn handle_output_key(&mut self, key: KeyEvent) {
        if self.commit_detail.is_some() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Backspace) => {
                    self.close_commit_detail();
                }
                (KeyModifiers::NONE, KeyCode::Up) => {
                    self.output_scroll = self.output_scroll.saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    self.output_scroll = self
                        .output_scroll
                        .saturating_add(1)
                        .min(self.output_max_scroll());
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    self.output_scroll = self.output_scroll.saturating_sub(6);
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    self.output_scroll = self
                        .output_scroll
                        .saturating_add(6)
                        .min(self.output_max_scroll());
                }
                (KeyModifiers::NONE, KeyCode::Home) => self.output_scroll = 0,
                (KeyModifiers::NONE, KeyCode::End) => self.output_scroll = self.output_max_scroll(),
                (KeyModifiers::NONE, KeyCode::Char('v')) => {
                    self.diff_split = !self.diff_split;
                    self.status_message = if self.diff_split {
                        "Commit patch: split view"
                    } else {
                        "Commit patch: unified view"
                    }
                    .to_owned();
                }
                (KeyModifiers::NONE, KeyCode::Char('c')) => {
                    self.diff_full_context = !self.diff_full_context;
                    self.reload_commit_detail_context();
                }
                _ => {}
            }
            return;
        }
        if self.pull_request_comment_mode {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char(character)) => {
                    let _ = edit_overlay_text(
                        &mut self.commit_message,
                        &mut self.commit_cursor,
                        "Char",
                        Some(character),
                    );
                }
                (KeyModifiers::NONE, KeyCode::Backspace) => {
                    let _ = edit_overlay_text(
                        &mut self.commit_message,
                        &mut self.commit_cursor,
                        "Backspace",
                        None,
                    );
                }
                (KeyModifiers::NONE, KeyCode::Delete)
                | (KeyModifiers::NONE, KeyCode::Left)
                | (KeyModifiers::NONE, KeyCode::Right)
                | (KeyModifiers::NONE, KeyCode::Home)
                | (KeyModifiers::NONE, KeyCode::End) => {
                    let key_name = match key.code {
                        KeyCode::Delete => "Delete",
                        KeyCode::Left => "Left",
                        KeyCode::Right => "Right",
                        KeyCode::Home => "Home",
                        KeyCode::End => "End",
                        _ => unreachable!(),
                    };
                    let _ = edit_overlay_text(
                        &mut self.commit_message,
                        &mut self.commit_cursor,
                        key_name,
                        None,
                    );
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.pull_request_comment_mode = false;
                    self.commit_message.clear();
                    self.commit_cursor = 0;
                    self.status_message = "Pull request comment cancelled".to_owned();
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    if self.commit_message.trim().is_empty() {
                        self.status_message = "Pull request comment must not be empty".to_owned();
                    } else if let Err(error) = self.submit_pull_request_comment() {
                        self.status_message = format!("PR comment failed: {error}");
                    }
                }
                _ => {}
            }
            return;
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Enter) if !self.history.is_empty() => {
                self.open_commit_detail();
            }
            (KeyModifiers::NONE, KeyCode::Char(']')) if !self.history.is_empty() => {
                self.load_more_history();
            }
            (KeyModifiers::NONE, KeyCode::Char('i'))
                if !self.plugin_registry_entries.is_empty() =>
            {
                let entry = self
                    .plugin_registry_entries
                    .get(self.output_scroll)
                    .cloned()
                    .unwrap_or_else(|| self.plugin_registry_entries[0].clone());
                let destination = self
                    .search_root
                    .join(".mtide")
                    .join("plugins")
                    .join(format!("{}-{}", entry.name, entry.version));
                match download_plugin_registry_entry(&entry)
                    .and_then(|bytes| install_plugin_archive(&bytes, &destination))
                {
                    Ok(package) => match load_plugin_runtime_for_workspace(
                        &package,
                        self.settings_path.as_deref(),
                        &self.workspace_roots,
                    ) {
                        Ok(runtime) => match runtime.run_install_hook() {
                            Ok(_) => {
                                self.plugin_runtimes.push(runtime);
                                self.status_message = format!(
                                    "Installed and loaded plugin {} {}",
                                    entry.name, entry.version
                                );
                            }
                            Err(error) => {
                                let _ = fs::remove_dir_all(&destination);
                                self.status_message = format!(
                                    "Installed plugin {}, install hook failed: {error}",
                                    entry.name
                                );
                            }
                        },
                        Err(error) => {
                            self.status_message = format!(
                                "Installed plugin {}, but loading failed: {error}",
                                entry.name
                            );
                        }
                    },
                    Err(error) => self.status_message = format!("Plugin install failed: {error}"),
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter)
                if self.diff_split
                    && !self.pull_request_comments_visible
                    && !self.change_diff.is_empty() =>
            {
                if self.expand_diff_context_at_scroll() {
                    self.status_message = "Expanded Git diff context gap".to_owned();
                    return;
                }
                if let Some(change) = self.changes.get(self.change_index).cloned() {
                    let root = self.change_root(self.change_index).to_path_buf();
                    let path = root.join(&change.path);
                    if path.is_file() {
                        match Buffer::open(&path) {
                            Ok(buffer) => {
                                self.open_buffer(buffer);
                                self.diff_base_lines = (change.index != '?'
                                    && change.worktree != '?')
                                    .then(|| git_base_file(&root, &change.path))
                                    .flatten();
                                self.focus = Focus::Editor;
                                self.status_message =
                                    format!("Opened {} for split diff editing", path.display());
                            }
                            Err(error) => {
                                self.status_message = format!("Open diff file failed: {error}")
                            }
                        }
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter)
                if !self.pull_request_comments_visible && !self.change_diff.is_empty() =>
            {
                if self.expand_diff_context_at_scroll() {
                    self.status_message = "Expanded Git diff context gap".to_owned();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('v')) if !self.change_diff.is_empty() => {
                self.diff_split = !self.diff_split;
                self.status_message = if self.diff_split {
                    "Git diff: split view"
                } else {
                    "Git diff: unified view"
                }
                .to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('c'))
                if !self.change_diff.is_empty() && self.pull_request_target.is_none() =>
            {
                self.diff_full_context = !self.diff_full_context;
                self.output_scroll = self.output_scroll.min(self.output_max_scroll());
                self.status_message = if self.diff_full_context {
                    "Git diff: full context"
                } else {
                    "Git diff: changes-only context"
                }
                .to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('e')) if !self.change_diff.is_empty() => {
                if self.expand_diff_context_at_scroll() {
                    self.status_message = "Expanded Git diff context gap".to_owned();
                } else {
                    self.diff_full_context = true;
                    self.status_message = "Expanded all Git diff context".to_owned();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('m')) => {
                if !self.pull_request_comments.is_empty() {
                    self.pull_request_comments_visible = !self.pull_request_comments_visible;
                    self.output_scroll = 0;
                    self.status_message = if self.pull_request_comments_visible {
                        "Pull request comments"
                    } else {
                        "Pull request diff"
                    }
                    .to_owned();
                } else if self.pull_request_target.is_some() {
                    self.pull_request_comments_visible = true;
                    if let Err(error) = self.load_pull_request_comments() {
                        self.status_message = format!("PR comments failed: {error}");
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter)
                if self.pull_request_comments_visible && !self.pull_request_comments.is_empty() =>
            {
                if let Err(error) = self.jump_to_pull_request_comment() {
                    self.status_message = format!("PR comment navigation failed: {error}");
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('c'))
                if self.pull_request_target.is_some() && !self.change_diff.is_empty() =>
            {
                self.pull_request_comment_mode = true;
                self.commit_message.clear();
                self.commit_cursor = 0;
                self.status_message =
                    "PR comment · type text · Enter submit · Esc cancel".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.output_scroll = self.output_scroll.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.output_scroll = self
                    .output_scroll
                    .saturating_add(1)
                    .min(self.output_max_scroll());
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.output_scroll = self.output_scroll.saturating_sub(6);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.output_scroll = self
                    .output_scroll
                    .saturating_add(6)
                    .min(self.output_max_scroll());
            }
            (KeyModifiers::NONE, KeyCode::Home) => self.output_scroll = 0,
            (KeyModifiers::NONE, KeyCode::End) => {
                self.output_scroll = self.output_max_scroll();
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.focus = Focus::Editor;
                self.status_message = "Returned to editor".to_owned();
            }
            _ => self.handle_navigation(key),
        }
    }

    fn resolve_selected_conflict(&mut self, ours: bool) -> AppResult<()> {
        let Some(change) = self.changes.get(self.change_index).cloned() else {
            return Ok(());
        };
        if !change.is_conflict() {
            self.status_message = "Selected change is not conflicted".to_owned();
            return Ok(());
        }
        let root = self.change_root(self.change_index).to_path_buf();
        checkout_conflict_side(&root, &change.path, ours)?;
        self.refresh_git_state();
        self.status_message = format!(
            "Resolved {} from {} · stage with s",
            change.path,
            if ours { "ours" } else { "theirs" }
        );
        Ok(())
    }

    fn resolve_current_conflict(&mut self, ours: bool) {
        let blocks = conflict_blocks(self.file_lines());
        let line = self.cursor().line;
        let Some(block) = blocks
            .iter()
            .find(|block| (block.start_line..=block.end_line).contains(&line))
            .or_else(|| blocks.first())
            .cloned()
        else {
            self.status_message = "No inline conflict block found".to_owned();
            return;
        };
        let replacement = if ours {
            self.file_lines()[block.ours_start..block.ours_end].to_vec()
        } else {
            self.file_lines()[block.theirs_start..block.theirs_end].to_vec()
        };
        self.buffer_mut()
            .replace_line_range(block.start_line, block.end_line + 1, &replacement);
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.status_message = format!(
            "Resolved inline conflict with {} · {} block(s) remain",
            if ours { "ours" } else { "theirs" },
            conflict_blocks(self.file_lines()).len()
        );
    }
    fn resolve_current_conflict_base(&mut self) {
        let blocks = conflict_blocks(self.file_lines());
        let line = self.cursor().line;
        let Some(block) = blocks
            .iter()
            .find(|block| (block.start_line..=block.end_line).contains(&line))
            .or_else(|| blocks.first())
            .cloned()
        else {
            self.status_message = "No inline conflict block found".to_owned();
            return;
        };
        let (Some(base_start), Some(base_end)) = (block.base_start, block.base_end) else {
            self.status_message = "Selected conflict has no base section".to_owned();
            return;
        };
        let replacement = self.file_lines()[base_start..base_end].to_vec();
        self.buffer_mut()
            .replace_line_range(block.start_line, block.end_line + 1, &replacement);
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.status_message = format!(
            "Resolved inline conflict with base · {} block(s) remain",
            conflict_blocks(self.file_lines()).len()
        );
    }
    fn resolve_current_conflict_both(&mut self) {
        let blocks = conflict_blocks(self.file_lines());
        let line = self.cursor().line;
        let Some(block) = blocks
            .iter()
            .find(|block| (block.start_line..=block.end_line).contains(&line))
            .or_else(|| blocks.first())
            .cloned()
        else {
            self.status_message = "No inline conflict block found".to_owned();
            return;
        };
        let mut replacement = self.file_lines()[block.ours_start..block.ours_end].to_vec();
        replacement.extend_from_slice(&self.file_lines()[block.theirs_start..block.theirs_end]);
        self.buffer_mut()
            .replace_line_range(block.start_line, block.end_line + 1, &replacement);
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.status_message = format!(
            "Resolved inline conflict with both sides · {} block(s) remain",
            conflict_blocks(self.file_lines()).len()
        );
    }

    fn jump_conflict(&mut self, forward: bool) {
        let blocks = conflict_blocks(self.file_lines());
        let Some(current) = blocks
            .iter()
            .position(|block| (block.start_line..=block.end_line).contains(&self.cursor().line))
        else {
            if let Some(block) = if forward {
                blocks.first()
            } else {
                blocks.last()
            } {
                self.buffer_mut().set_cursor(block.start_line, 0);
                self.status_message = format!(
                    "Conflict block 1/{} · Ours/Theirs/Base available",
                    blocks.len()
                );
            } else {
                self.status_message = "No inline conflict block found".to_owned();
            }
            return;
        };
        let next = if forward {
            (current + 1) % blocks.len()
        } else {
            current.checked_sub(1).unwrap_or(blocks.len() - 1)
        };
        self.buffer_mut().set_cursor(blocks[next].start_line, 0);
        self.status_message = format!(
            "Conflict block {}/{} · Ours/Theirs/Base available",
            next + 1,
            blocks.len()
        );
    }

    fn handle_commit_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                let _ = edit_overlay_text(
                    &mut self.commit_message,
                    &mut self.commit_cursor,
                    "Char",
                    Some(character),
                );
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                let _ = edit_overlay_text(
                    &mut self.commit_message,
                    &mut self.commit_cursor,
                    "Backspace",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Delete)
            | (KeyModifiers::NONE, KeyCode::Left)
            | (KeyModifiers::NONE, KeyCode::Right)
            | (KeyModifiers::NONE, KeyCode::Home)
            | (KeyModifiers::NONE, KeyCode::End) => {
                let key_name = match key.code {
                    KeyCode::Delete => "Delete",
                    KeyCode::Left => "Left",
                    KeyCode::Right => "Right",
                    KeyCode::Home => "Home",
                    KeyCode::End => "End",
                    _ => unreachable!(),
                };
                let _ = edit_overlay_text(
                    &mut self.commit_message,
                    &mut self.commit_cursor,
                    key_name,
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.commit_mode = false;
                self.commit_message.clear();
                self.commit_cursor = 0;
                self.status_message = "Commit cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let root = self.active_git_root();
                match git_commit(&root, &self.commit_message) {
                    Ok(()) => {
                        self.commit_mode = false;
                        self.commit_message.clear();
                        self.commit_cursor = 0;
                        self.refresh_git_state();
                        self.status_message = "Committed staged changes".to_owned();
                    }
                    Err(error) => self.status_message = format!("Commit failed: {error}"),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn load_blame(&mut self) {
        self.close_commit_detail();
        let Some(path) = self.buffer().path() else {
            self.status_message = "Blame unavailable for welcome buffer".to_owned();
            return;
        };
        let root = self.active_git_root();
        let Ok(relative) = path.strip_prefix(&root) else {
            self.status_message = "Blame file is outside workspace".to_owned();
            return;
        };
        let relative = relative.to_string_lossy().into_owned();
        let generation = self.git_blame_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation_state = Arc::clone(&self.git_blame_generation);
        let sender = self.git_blame_tx.clone();
        self.blame_lines.clear();
        self.bottom_tab = BottomTab::Output;
        let worker_root = root.clone();
        let worker_relative = relative.clone();
        if std::thread::Builder::new()
            .name("mtide-git-blame".to_owned())
            .spawn(move || {
                let lines = git_blame(&worker_root, &worker_relative);
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(GitBlameCompletion {
                        generation,
                        root: worker_root,
                        relative: worker_relative,
                        lines,
                    });
                }
            })
            .is_err()
        {
            self.status_message = "Unable to start Git blame".to_owned();
        } else {
            self.status_message = "Git blame running…".to_owned();
        }
    }

    fn load_history(&mut self) {
        self.close_commit_detail();
        let root = self.active_git_root();
        self.git_blame_generation.fetch_add(1, Ordering::AcqRel);
        self.blame_lines.clear();
        self.history.clear();
        self.history_snapshot_head = None;
        self.history_root = Some(root.clone());
        self.history_offset = 0;
        self.history_has_more = false;
        self.history_pending = true;
        let generation = self.git_history_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation_state = Arc::clone(&self.git_history_generation);
        let sender = self.git_history_tx.clone();
        let worker_root = root.clone();
        if std::thread::Builder::new()
            .name("mtide-git-history".to_owned())
            .spawn(move || {
                let (commits, has_more, snapshot_head) =
                    git_history_page(&worker_root, 0, 10, None);
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(GitHistoryCompletion {
                        generation,
                        root: worker_root,
                        commits,
                        snapshot_head,
                        append: false,
                        has_more,
                    });
                }
            })
            .is_err()
        {
            self.status_message = "Unable to start Git history".to_owned();
        } else {
            self.status_message = "Git history running…".to_owned();
        }
        self.bottom_tab = BottomTab::Output;
    }

    fn load_more_history(&mut self) {
        if self.history_pending {
            self.status_message = "Git history is still loading".to_owned();
            return;
        }
        if !self.history_has_more {
            self.status_message = "No older commits".to_owned();
            return;
        }
        let Some(root) = self.history_root.clone() else {
            self.status_message = "Git history repository is unavailable".to_owned();
            self.history_has_more = false;
            return;
        };
        let snapshot_head = self.history_snapshot_head.clone();
        if snapshot_head.is_none() {
            self.status_message = "Git history snapshot is unavailable".to_owned();
            self.history_has_more = false;
            return;
        }
        self.history_pending = true;
        let skip = self.history_offset;
        let generation = self.git_history_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation_state = Arc::clone(&self.git_history_generation);
        let sender = self.git_history_tx.clone();
        let worker_root = root.clone();
        let worker_snapshot_head = snapshot_head.clone();
        if std::thread::Builder::new()
            .name("mtide-git-history-page".to_owned())
            .spawn(move || {
                let (commits, has_more, _) =
                    git_history_page(&worker_root, skip, 10, worker_snapshot_head.as_deref());
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(GitHistoryCompletion {
                        generation,
                        root: worker_root,
                        commits,
                        snapshot_head: worker_snapshot_head,
                        append: true,
                        has_more,
                    });
                }
            })
            .is_err()
        {
            self.history_pending = false;
            self.status_message = "Unable to start older Git history".to_owned();
        } else {
            self.status_message = format!("Loading older commits from {}…", skip + 1);
        }
    }

    fn run_git_remote(&mut self, operation: &str) {
        if self.git_remote_pending.is_some() {
            self.status_message = "A Git remote operation is already running".to_owned();
            return;
        }
        if !matches!(operation, "fetch" | "pull" | "push" | "sync") {
            return;
        }
        let root = self.active_git_root();
        let operation = operation.to_owned();
        let sender = self.git_remote_tx.clone();
        let worker_operation = operation.clone();
        if std::thread::Builder::new()
            .name("mtide-git-remote".to_owned())
            .spawn(move || {
                let result = match worker_operation.as_str() {
                    "fetch" => git_fetch(&root),
                    "pull" => git_pull(&root),
                    "push" => git_push(&root),
                    "sync" => git_sync(&root),
                    _ => unreachable!("validated Git remote operation"),
                }
                .map_err(|error| error.to_string());
                let _ = sender.send(GitRemoteCompletion {
                    operation: worker_operation,
                    root,
                    result,
                });
            })
            .is_err()
        {
            self.status_message = format!("Unable to start Git {operation}");
            return;
        }
        self.git_remote_pending = Some(operation.clone());
        self.status_message = format!("Git {operation} running…");
    }

    fn handle_git_remote_failure(&mut self, operation: &str, root: &Path, message: &str) {
        let lower = message.to_ascii_lowercase();
        if lower.contains("could not read username")
            || lower.contains("authentication failed")
            || lower.contains("authentication required")
            || lower.contains("terminal prompts disabled")
            || lower.contains("http 401")
            || lower.contains("http 403")
        {
            self.git_credential_prompt = Some(GitCredentialPrompt {
                operation: operation.to_owned(),
                root: root.to_path_buf(),
                username: String::new(),
                password: String::new(),
                username_cursor: 0,
                password_cursor: 0,
                password_focus: false,
            });
            self.status_message =
                "Git credentials · Tab password · Enter retry · Esc cancel".to_owned();
        } else {
            self.status_message = format!("Git {operation} failed: {message}");
        }
    }
    fn handle_branches_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.branch_mode {
            return self.handle_branch_name_key(key);
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('n' | 'c')) => {
                self.branch_mode = true;
                self.branch_name.clear();
                self.branch_cursor = 0;
                self.status_message = "New branch · Enter create · Esc cancel".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                if let Some(branch) = self.branches.get(self.branch_index).cloned() {
                    if self.branch.as_deref() == Some(branch.as_str()) {
                        self.status_message = "Cannot delete the current branch".to_owned();
                    } else {
                        let root = self.active_git_root();
                        match git_delete_branch(&root, &branch) {
                            Ok(()) => {
                                self.refresh_git_state();
                                self.status_message = format!("Deleted branch {branch}");
                            }
                            Err(error) => {
                                self.status_message = format!("Branch delete failed: {error}");
                            }
                        }
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if !self.branches.is_empty() {
                    self.branch_index = (self.branch_index + 1).min(self.branches.len() - 1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.branch_index = self.branch_index.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(branch) = self.branches.get(self.branch_index).cloned() {
                    let root = self.active_git_root();
                    match git_checkout(&root, &branch) {
                        Ok(()) => {
                            self.refresh_git_state();
                            self.explorer_paths = self.explorer_entries();
                            self.refresh_buffers_after_workspace_change();
                            self.status_message = format!("Switched to {branch}");
                        }
                        Err(error) => {
                            self.status_message = format!("Branch switch failed: {error}");
                        }
                    }
                }
            }
            _ => self.handle_navigation(key),
        }
        Ok(())
    }

    fn refresh_buffers_after_workspace_change(&mut self) {
        let mut active_reloaded = false;
        let mut any_reloaded = false;
        let mut dirty_changed = false;
        let mut deleted_changed = false;
        for index in 0..self.buffers.len() {
            if self.buffers[index].dirty() {
                match self.buffers[index].poll_external_change() {
                    Ok(Some(ExternalChange::Dirty)) => {
                        dirty_changed = true;
                    }
                    Ok(Some(ExternalChange::Deleted)) => {
                        deleted_changed = true;
                    }
                    Ok(Some(ExternalChange::Reloaded)) | Ok(None) => {}
                    Err(error) => {
                        self.status_message = format!("Workspace refresh failed: {error}");
                    }
                }
                continue;
            }
            match self.buffers[index].reload_from_disk() {
                Ok(true) => {
                    any_reloaded = true;
                    if index == self.active_tab {
                        active_reloaded = true;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    self.status_message = format!("Workspace refresh failed: {error}");
                }
            }
        }
        if any_reloaded {
            self.refresh_search_results();
        }
        if active_reloaded {
            self.sync_lsp_for_buffer();
            self.emit_plugin_event("document.reload");
        }
        if dirty_changed {
            self.status_message = "Branch switched; unsaved buffer changed on disk".to_owned();
        } else if deleted_changed {
            self.status_message = "Branch switched; an open file was deleted on disk".to_owned();
        }
    }
    fn handle_branch_name_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                let _ = edit_overlay_text(
                    &mut self.branch_name,
                    &mut self.branch_cursor,
                    "Char",
                    Some(character),
                );
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                let _ = edit_overlay_text(
                    &mut self.branch_name,
                    &mut self.branch_cursor,
                    "Backspace",
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Delete)
            | (KeyModifiers::NONE, KeyCode::Left)
            | (KeyModifiers::NONE, KeyCode::Right)
            | (KeyModifiers::NONE, KeyCode::Home)
            | (KeyModifiers::NONE, KeyCode::End) => {
                let key_name = match key.code {
                    KeyCode::Delete => "Delete",
                    KeyCode::Left => "Left",
                    KeyCode::Right => "Right",
                    KeyCode::Home => "Home",
                    KeyCode::End => "End",
                    _ => unreachable!(),
                };
                let _ = edit_overlay_text(
                    &mut self.branch_name,
                    &mut self.branch_cursor,
                    key_name,
                    None,
                );
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.branch_mode = false;
                self.branch_name.clear();
                self.branch_cursor = 0;
                self.status_message = "Branch creation cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.branch_name.trim().is_empty() {
                    self.status_message = "Branch name must not be empty".to_owned();
                    return Ok(());
                }
                let branch = self.branch_name.trim().to_owned();
                let root = self.active_git_root();
                match git_create_branch(&root, &branch) {
                    Ok(()) => {
                        self.branch_mode = false;
                        self.branch_name.clear();
                        self.branch_cursor = 0;
                        self.refresh_git_state();
                        self.status_message = format!("Created and switched to {branch}");
                    }
                    Err(error) => self.status_message = format!("Branch create failed: {error}"),
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn refresh_change_diff(&mut self) {
        self.change_diff = self
            .changes
            .get(self.change_index)
            .map_or_else(Vec::new, |change| {
                GitBackend::new(self.change_root(self.change_index)).diff(change)
            });
        self.diff_expanded_context.clear();
        self.change_hunk_index = self
            .change_hunk_index
            .min(git_hunk_count(&self.change_diff).saturating_sub(1));
        self.output_scroll = 0;
    }

    fn change_hunk_stage(&mut self, should_stage: bool) -> AppResult<()> {
        if self.git_state_pending {
            self.status_message = "Git refresh is still running".to_owned();
            return Ok(());
        }
        let Some(change) = self.changes.get(self.change_index).cloned() else {
            return Ok(());
        };
        let root = self.change_root(self.change_index).to_path_buf();
        git_apply_hunk(&root, &change, self.change_hunk_index, should_stage)?;
        self.refresh_git_state();
        self.status_message = format!(
            "{} hunk {}/{} for {}",
            if should_stage { "Staged" } else { "Unstaged" },
            self.change_hunk_index + 1,
            self.change_hunk_count().max(self.change_hunk_index + 1),
            change.path
        );
        Ok(())
    }
    fn change_stage_all(&mut self, should_stage: bool) -> AppResult<()> {
        if self.git_state_pending {
            self.status_message = "Git refresh is still running".to_owned();
            return Ok(());
        }
        let changes = self.changes.clone();
        let mut changed = 0usize;
        for (index, change) in changes.into_iter().enumerate() {
            let root = self.change_root(index).to_path_buf();
            let should_apply = if should_stage {
                change.worktree != ' ' || change.index == '?'
            } else {
                change.index != ' ' && change.index != '?'
            };
            if !should_apply {
                continue;
            }
            if should_stage {
                git_stage(&root, &change.path)?;
            } else {
                git_unstage(&root, &change.path)?;
            }
            changed += 1;
        }
        self.refresh_git_state();
        self.status_message = format!(
            "{} {} file(s)",
            if should_stage { "Staged" } else { "Unstaged" },
            changed
        );
        Ok(())
    }
    fn sync_lsp_for_buffer(&mut self) {
        if !self.settings.lsp.enabled {
            return;
        }
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            return;
        };
        let language = self.language().to_owned();
        if language.is_empty() {
            return;
        }
        if !self.lsp.configured(&language) {
            return;
        }
        let uri = path_to_uri(&path);
        let root_uri = path_to_uri(&self.active_git_root());
        match self.lsp.start(&language, Some(&root_uri)) {
            Ok(true) => {
                let text = self.buffer().lines().join("\n");
                if let Err(error) = self.lsp.sync_document(&language, &uri, &language, &text) {
                    self.status_message = format!("LSP sync failed: {error}");
                }
            }
            Ok(false) => {}
            Err(error) => self.status_message = format!("LSP start failed: {error}"),
        }
    }

    fn request_ai_completion(&mut self) {
        if !self.settings.ai.enabled {
            return;
        }
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            return;
        };
        let cursor = self.buffer().cursor();
        let lines = self.file_lines();
        let max_lines = self.settings.ai.max_context_lines.clamp(8, 240);
        let start = cursor.line.saturating_sub(max_lines / 2);
        let end = (start + max_lines).min(lines.len());
        let text = lines[start..end].join("\n");
        let prefix = lines
            .get(cursor.line)
            .map(|line| line.chars().take(cursor.column).collect::<String>())
            .unwrap_or_default();
        let request = crate::ai::CompletionRequest {
            language: self.language().to_owned(),
            file_path: path.to_string_lossy().into_owned(),
            line: cursor.line + 1,
            column: cursor.column + 1,
            prefix,
            text,
        };
        let settings = self.settings.ai.clone();
        let generation = self.ai_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let tx = self.ai_tx.clone();
        self.status_message = "AI completion running…".to_owned();
        std::thread::spawn(move || {
            let result =
                crate::ai::complete(&settings, &request).map_err(|error| error.to_string());
            let _ = tx.send(AiCompletion {
                generation,
                path,
                cursor,
                result,
            });
        });
    }

    fn poll_ai_completions(&mut self) {
        while let Ok(completion) = self.ai_rx.try_recv() {
            if completion.generation != self.ai_generation.load(Ordering::Acquire)
                || self.buffer().path() != Some(completion.path.as_path())
                || self.buffer().cursor() != completion.cursor
                || self.lsp_popup.is_some()
            {
                continue;
            }
            match completion.result {
                Ok(text) if !text.is_empty() => {
                    self.lsp_popup = Some(LspPopup::Completion {
                        items: vec![LspCompletionItem {
                            label: format!("AI · {}", text.lines().next().unwrap_or("completion")),
                            insert_text: text,
                            edit_range: None,
                            additional_edits: Vec::new(),
                            commit_characters: Vec::new(),
                            resolve_params: None,
                        }],
                        selected: 0,
                    });
                    self.begin_overlay();
                    self.status_message = "AI completion ready".to_owned();
                }
                Ok(_) => self.status_message = "AI returned no completion".to_owned(),
                Err(error) => self.status_message = format!("AI completion unavailable: {error}"),
            }
        }
    }

    fn save_active_buffer_now(&mut self, editor: &crate::config::EditorSettings) -> AppResult<()> {
        if self.buffer().has_unresolved_conflicts() {
            self.status_message = "Save blocked: resolve merge conflicts first".to_owned();
            return Ok(());
        }
        if self.buffer().has_external_change() {
            self.status_message =
                "Save blocked: reload or Save As before overwriting disk changes".to_owned();
            return Ok(());
        }
        self.buffer_mut()
            .save_with_options(editor.trim_trailing_whitespace, editor.insert_final_newline)?;
        if let Some(path) = self.buffer().path().map(Path::to_path_buf) {
            let language = self.language().to_owned();
            self.notify_lsp_saved(&language, &path);
        }
        self.emit_plugin_event("document.save");
        self.refresh_git_state();
        self.status_message = "Saved".to_owned();
        Ok(())
    }

    fn formatter_configured(&self) -> bool {
        self.buffer()
            .path()
            .and_then(|path| path.extension())
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| self.settings.formatters.contains_key(extension))
    }

    fn run_external_formatter(&mut self) -> AppResult<()> {
        let path = self.buffer().path().map(Path::to_path_buf).ok_or_else(|| {
            crate::error::AppError::Workspace("formatting requires a file path".into())
        })?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .ok_or_else(|| crate::error::AppError::Workspace("file has no extension".into()))?;
        let configured = self
            .settings
            .formatters
            .get(extension)
            .ok_or_else(|| {
                crate::error::AppError::Workspace(format!(
                    "no formatter configured for .{extension}"
                ))
            })?
            .trim();
        if configured.is_empty() {
            return Err(crate::error::AppError::Workspace(format!(
                "empty formatter command for .{extension}"
            )));
        }
        let command = configured.replace("{file}", &shell_quote(&path.to_string_lossy()));
        let input = self.buffer().lines().join("\n");
        let mut child = Command::new("sh")
            .args(["-c", &command])
            .current_dir(path.parent().unwrap_or_else(|| Path::new(".")))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(crate::error::AppError::Workspace(format!(
                "formatter exited with {}{}",
                output.status,
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            )));
        }
        let formatted = String::from_utf8(output.stdout).map_err(|error| {
            crate::error::AppError::Workspace(format!("formatter output is not UTF-8: {error}"))
        })?;
        self.buffer_mut()
            .replace_document_text(formatted.trim_end_matches('\n'));
        self.status_message = format!("Formatted .{extension} with external formatter");
        self.sync_lsp_for_buffer();
        self.emit_plugin_event("editor.change");
        Ok(())
    }

    fn restart_lsp(&mut self) -> AppResult<()> {
        if !self.settings.lsp.enabled {
            self.status_message = "LSP is disabled".to_owned();
            return Ok(());
        }
        let language = self.language().to_owned();
        if language.is_empty() {
            self.status_message = "LSP restart unavailable: unknown language".to_owned();
            return Ok(());
        }
        if !self.lsp.configured(&language) {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        }
        let root_uri = path_to_uri(&self.active_git_root());
        match self.lsp.restart(&language, Some(&root_uri)) {
            Ok(true) => {
                self.sync_lsp_for_buffer();
                self.status_message = format!("Restarted LSP server for {language}");
            }
            Ok(false) => self.status_message = format!("No LSP server configured for {language}"),
            Err(error) => self.status_message = format!("LSP restart failed: {error}"),
        }
        Ok(())
    }

    fn notify_lsp_saved(&mut self, language: &str, path: &Path) {
        if !self.settings.lsp.enabled || language.is_empty() || !self.lsp.configured(language) {
            return;
        }
        let uri = path_to_uri(path);
        if let Err(error) = self.lsp.save_document(language, &uri) {
            self.status_message = format!("LSP save notification failed: {error}");
        }
    }

    fn notify_lsp_closed(&mut self, language: &str, path: &Path) {
        if !self.settings.lsp.enabled || language.is_empty() || !self.lsp.configured(language) {
            return;
        }
        let uri = path_to_uri(path);
        if let Err(error) = self.lsp.close_document(language, &uri) {
            self.status_message = format!("LSP close notification failed: {error}");
        }
    }

    fn change_stage(&mut self, should_stage: bool) -> AppResult<()> {
        if self.git_state_pending {
            self.status_message = "Git refresh is still running".to_owned();
            return Ok(());
        }
        let Some(path) = self
            .changes
            .get(self.change_index)
            .map(|change| change.path.clone())
        else {
            return Ok(());
        };
        let root = self.change_root(self.change_index).to_path_buf();
        if should_stage {
            git_stage(&root, &path)?;
            self.status_message = format!("Staged {path}");
        } else {
            git_unstage(&root, &path)?;
            self.status_message = format!("Unstaged {path}");
        }
        self.refresh_git_state();
        Ok(())
    }
    fn refresh_git_state(&mut self) {
        self.git_state_pending = true;
        let roots = self.workspace_roots.clone();
        let active_root = self.active_git_root();
        let selected_index = self.change_index;
        let generation = self.git_state_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation_state = Arc::clone(&self.git_state_generation);
        let sender = self.git_state_tx.clone();
        if std::thread::Builder::new()
            .name("mtide-git-state".to_owned())
            .spawn(move || {
                let (changes, change_roots) = workspace_git_changes(&roots);
                let changes_by_root = roots
                    .iter()
                    .map(|root| (root.clone(), git_status(root)))
                    .collect();
                let branch = git_branch(&active_root);
                let branches = git_branches(&active_root);
                let change_diff = changes.get(selected_index).map_or_else(Vec::new, |change| {
                    GitBackend::new(change_roots.get(selected_index).unwrap_or(&active_root))
                        .diff(change)
                });
                if generation_state.load(Ordering::Acquire) == generation {
                    let _ = sender.send(GitStateCompletion {
                        generation,
                        active_root,
                        changes,
                        change_roots,
                        changes_by_root,
                        branch,
                        branches,
                        change_diff,
                    });
                }
            })
            .is_err()
        {
            self.git_state_pending = false;
            self.status_message = "Unable to start Git refresh".to_owned();
        } else {
            self.status_message = "Refreshing Git…".to_owned();
        }
    }
    fn handle_explorer_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.sidebar_tab == SidebarTab::Problems {
            let problems = self.problems();
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Down) => {
                    self.problem_index =
                        (self.problem_index + 1).min(problems.len().saturating_sub(1));
                }
                (KeyModifiers::NONE, KeyCode::Up) => {
                    self.problem_index = self.problem_index.saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    self.activate_problem(self.problem_index);
                }
                _ => self.handle_navigation(key),
            }
            return Ok(());
        }
        if self.explorer_mode.is_some() {
            return self.handle_explorer_mode_key(key);
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Down) => {
                if !self.explorer_paths.is_empty() {
                    self.explorer_index =
                        (self.explorer_index + 1).min(self.explorer_paths.len() - 1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.explorer_index = self.explorer_index.saturating_sub(1)
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(path) = self.explorer_paths.get(self.explorer_index).cloned() {
                    if path.is_dir() {
                        self.toggle_explorer_directory(&path);
                    } else {
                        if !self.open_plugin_file(&path)? {
                            self.open_buffer(Buffer::open(&path)?);
                        }
                        self.sync_lsp_for_buffer();
                        self.emit_plugin_event("document.open");
                        self.status_message = format!("Opened {}", path.display());
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if let Some(parent) = self.search_root.parent().map(Path::to_path_buf) {
                    self.search_root = parent;
                    self.explorer_paths = self.explorer_entries();
                    self.explorer_index = 0;
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('n')) => self.start_explorer_input(false),
            (KeyModifiers::SHIFT, KeyCode::Char('N')) => self.start_explorer_input(true),
            (KeyModifiers::NONE, KeyCode::Char('r')) => self.start_explorer_rename(),
            (KeyModifiers::NONE, KeyCode::Char('x')) => self.start_explorer_delete(),
            (_, KeyCode::F(5)) => self.refresh_explorer(),
            _ => self.handle_navigation(key),
        }
        Ok(())
    }

    fn handle_explorer_mode_key(&mut self, key: KeyEvent) -> AppResult<()> {
        let mode = self.explorer_mode.clone().expect("checked above");
        if matches!(mode, ExplorerMode::ConfirmDelete(_)) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.delete_selected_path()?,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.cancel_explorer_input()
                }
                _ => {}
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Char(character) => {
                let _ = edit_overlay_text(
                    &mut self.explorer_input,
                    &mut self.explorer_cursor,
                    "Char",
                    Some(character),
                );
            }
            KeyCode::Backspace => {
                let _ = edit_overlay_text(
                    &mut self.explorer_input,
                    &mut self.explorer_cursor,
                    "Backspace",
                    None,
                );
            }
            KeyCode::Delete | KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => {
                let key_name = match key.code {
                    KeyCode::Delete => "Delete",
                    KeyCode::Left => "Left",
                    KeyCode::Right => "Right",
                    KeyCode::Home => "Home",
                    KeyCode::End => "End",
                    _ => unreachable!(),
                };
                let _ = edit_overlay_text(
                    &mut self.explorer_input,
                    &mut self.explorer_cursor,
                    key_name,
                    None,
                );
            }
            KeyCode::Esc => self.cancel_explorer_input(),
            KeyCode::Enter => self.commit_explorer_input()?,
            _ => {}
        }
        Ok(())
    }

    fn start_explorer_input(&mut self, directory: bool) {
        self.explorer_input.clear();
        self.explorer_cursor = 0;
        self.explorer_mode = Some(if directory {
            ExplorerMode::CreateDirectory
        } else {
            ExplorerMode::CreateFile
        });
        self.status_message = format!(
            "{} · Enter apply · Esc cancel",
            self.explorer_mode.as_ref().expect("just set").title()
        );
    }

    fn start_explorer_rename(&mut self) {
        let Some(path) = self.selected_explorer_path() else {
            self.status_message = "No Explorer item selected".to_owned();
            return;
        };
        self.explorer_input = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        self.explorer_cursor = self.explorer_input.chars().count();
        self.explorer_mode = Some(ExplorerMode::Rename(path));
        self.status_message = "Rename · Enter apply · Esc cancel".to_owned();
    }

    fn start_explorer_delete(&mut self) {
        let Some(path) = self.selected_explorer_path() else {
            self.status_message = "No Explorer item selected".to_owned();
            return;
        };
        self.explorer_input.clear();
        self.explorer_cursor = 0;
        self.explorer_mode = Some(ExplorerMode::ConfirmDelete(path.clone()));
        self.status_message = format!("Delete {}? y/N", path.display());
    }

    fn selected_explorer_path(&self) -> Option<PathBuf> {
        self.explorer_paths.get(self.explorer_index).cloned()
    }

    fn explorer_creation_root(&self) -> PathBuf {
        self.selected_explorer_path()
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| self.search_root.clone())
    }

    fn relocate_open_buffers(&mut self, old: &Path, new: &Path) {
        let old_key = path_key(old);
        let override_prefix = format!("{old_key}{}", std::path::MAIN_SEPARATOR);
        let moved_overrides = self
            .language_overrides
            .iter()
            .filter_map(|(path, language)| {
                let relative = if path == &old_key {
                    Some(PathBuf::new())
                } else {
                    path.strip_prefix(&override_prefix).map(PathBuf::from)
                }?;
                let relocated = if relative.as_os_str().is_empty() {
                    new.to_path_buf()
                } else {
                    new.join(&relative)
                };
                Some((path.clone(), path_key(&relocated), language.clone()))
            })
            .collect::<Vec<_>>();
        for (old_path, _, _) in &moved_overrides {
            self.language_overrides.remove(old_path);
        }
        for (_, new_path, language) in moved_overrides {
            self.language_overrides.insert(new_path, language);
        }
        for buffer in &mut self.buffers {
            let Some(path) = buffer.path().map(Path::to_path_buf) else {
                continue;
            };
            let Some(relative) = path.strip_prefix(old).ok() else {
                continue;
            };
            let relocated = new.join(relative);
            buffer.relocate(relocated);
        }
        self.save_language_overrides();
    }

    fn commit_explorer_input(&mut self) -> AppResult<()> {
        let mode = self.explorer_mode.clone().expect("input mode");
        let name = self.explorer_input.trim();
        if name.is_empty() {
            self.status_message = "Name must not be empty".to_owned();
            return Ok(());
        }
        if Path::new(name).is_absolute() || name.contains('/') || name.contains('\\') {
            self.status_message = "Use a single relative file name".to_owned();
            return Ok(());
        }
        match mode {
            ExplorerMode::CreateFile => {
                let path = self.explorer_creation_root().join(name);
                if path.exists() {
                    self.status_message = format!("Already exists: {}", path.display());
                } else {
                    fs::File::create(&path)?;
                    self.refresh_explorer();
                    self.status_message = format!("Created {}", path.display());
                }
            }
            ExplorerMode::CreateDirectory => {
                let path = self.explorer_creation_root().join(name);
                if path.exists() {
                    self.status_message = format!("Already exists: {}", path.display());
                } else {
                    fs::create_dir(&path)?;
                    self.refresh_explorer();
                    self.status_message = format!("Created {}", path.display());
                }
            }
            ExplorerMode::Rename(path) => {
                let target = path.parent().unwrap_or_else(|| Path::new(".")).join(name);
                if target.exists() && target != path {
                    self.status_message = format!("Already exists: {}", target.display());
                } else {
                    let active_old_path = self
                        .buffer()
                        .path()
                        .filter(|open| open.strip_prefix(&path).is_ok())
                        .map(Path::to_path_buf);
                    let active_language =
                        active_old_path.as_ref().map(|_| self.language().to_owned());
                    if let (Some(language), Some(old_path)) =
                        (active_language.as_deref(), active_old_path.as_deref())
                    {
                        self.notify_lsp_closed(language, old_path);
                    }
                    fs::rename(&path, &target)?;
                    self.relocate_open_buffers(&path, &target);
                    if active_old_path.is_some() {
                        self.sync_lsp_for_buffer();
                    }
                    self.refresh_explorer();
                    self.status_message = format!("Renamed to {}", target.display());
                }
            }
            ExplorerMode::ConfirmDelete(_) => unreachable!("delete uses confirmation handler"),
        }
        self.cancel_explorer_input();
        Ok(())
    }

    fn delete_selected_path(&mut self) -> AppResult<()> {
        let Some(ExplorerMode::ConfirmDelete(path)) = self.explorer_mode.clone() else {
            return Ok(());
        };
        if path.is_dir() {
            fs::remove_dir(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        self.refresh_explorer();
        self.cancel_explorer_input();
        self.status_message = format!("Deleted {}", path.display());
        Ok(())
    }

    fn cancel_explorer_input(&mut self) {
        self.explorer_mode = None;
        self.explorer_input.clear();
        self.explorer_cursor = 0;
    }

    fn explorer_entries(&self) -> Vec<PathBuf> {
        let roots = if self.explorer_roots.len() > 1 {
            self.explorer_roots.clone()
        } else {
            vec![self
                .explorer_roots
                .first()
                .cloned()
                .unwrap_or_else(|| self.search_root.clone())]
        };
        roots
            .iter()
            .flat_map(|root| {
                visible_explorer_entries(
                    root,
                    &self.explorer_expanded,
                    self.settings.explorer.show_hidden,
                    self.settings.explorer.show_git_ignored,
                )
            })
            .collect()
    }

    fn toggle_explorer_directory(&mut self, path: &Path) {
        if !self.explorer_expanded.remove(path) {
            self.explorer_expanded.insert(path.to_path_buf());
        }
        self.refresh_explorer();
        self.status_message = if self.explorer_expanded.contains(path) {
            format!("Expanded {}", path.display())
        } else {
            format!("Collapsed {}", path.display())
        };
    }

    fn refresh_explorer(&mut self) {
        self.explorer_paths = self.explorer_entries();
        self.explorer_index = self
            .explorer_index
            .min(self.explorer_paths.len().saturating_sub(1));
    }

    fn reveal_active_file(&mut self) {
        if !self.settings.explorer.auto_reveal {
            return;
        }
        let Some(active) = self.buffer().path().map(Path::to_path_buf) else {
            return;
        };
        let Some(root) = self
            .explorer_roots
            .iter()
            .find(|root| active.starts_with(root))
            .cloned()
        else {
            return;
        };
        let mut directory = if active.is_dir() {
            active.clone()
        } else {
            active.parent().unwrap_or(&root).to_path_buf()
        };
        self.explorer_expanded.insert(root.clone());
        while directory.starts_with(&root) && directory != root {
            self.explorer_expanded.insert(directory.clone());
            let Some(parent) = directory.parent() else {
                break;
            };
            directory = parent.to_path_buf();
        }
        self.refresh_explorer();
        if let Some(index) = self.explorer_paths.iter().position(|path| path == &active) {
            self.explorer_index = index;
        }
    }

    fn handle_terminal_key(&mut self, key: KeyEvent) -> AppResult<()> {
        // Preserve the host's explicit fullscreen force-key before forwarding
        // ordinary Alt+character input to the PTY.
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('t') {
            self.toggle_terminal_fullscreen();
            return Ok(());
        }
        if matches!(key.code, KeyCode::Char('t' | 'T'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            self.open_terminal();
            return Ok(());
        }
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            self.copy_terminal_selection();
            return Ok(());
        }
        if key.code == KeyCode::Left
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            self.switch_terminal(-1);
            return Ok(());
        }
        if key.code == KeyCode::Right
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            self.switch_terminal(1);
            return Ok(());
        }
        if key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('v')
            && self.settings.terminal.ctrl_v
        {
            let clipboard = self.clipboard.clone();
            if !clipboard.is_empty() {
                if self.terminal_bracketed_paste() {
                    let mut payload = Vec::with_capacity(clipboard.len() + 12);
                    payload.extend_from_slice(b"\x1b[200~");
                    payload.extend_from_slice(clipboard.as_bytes());
                    payload.extend_from_slice(b"\x1b[201~");
                    self.terminal_mut().write_bytes(&payload)?;
                } else {
                    self.terminal_mut().write_bytes(clipboard.as_bytes())?;
                }
            }
            return Ok(());
        }
        if let Some(bytes) = terminal_modified_key(key) {
            self.terminal_mut().write_bytes(&bytes)?;
            return Ok(());
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.write_terminal_cursor_key(b"\x1b[H", b"\x1bOH")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.write_terminal_cursor_key(b"\x1b[F", b"\x1bOF")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Insert) => {
                self.terminal_mut().write_bytes(b"\x1b[2~")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                self.terminal_mut().write_bytes(b"\x1b[3~")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.terminal_mut().write_bytes(b"\x1b")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::F(number)) if (1..=12).contains(&number) => {
                self.terminal_mut().write_bytes(vt_function_key(number))?;
                return Ok(());
            }
            _ => {}
        }
        match key.code {
            KeyCode::PageUp => {
                if self.terminal_alternate_screen() {
                    self.terminal_mut().write_bytes(b"\x1b[5~")?;
                } else {
                    self.terminal_mut().scroll(3);
                }
                return Ok(());
            }
            KeyCode::PageDown => {
                if self.terminal_alternate_screen() {
                    self.terminal_mut().write_bytes(b"\x1b[6~")?;
                } else {
                    self.terminal_mut().scroll(-3);
                }
                return Ok(());
            }
            _ => {}
        }
        if self.terminal_alternate_screen() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char(character)) => {
                    let mut bytes = [0_u8; 4];
                    self.terminal_mut()
                        .write_bytes(character.encode_utf8(&mut bytes).as_bytes())?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Backspace) => {
                    self.terminal_mut().write_bytes(b"\x7f")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    self.terminal_mut().write_bytes(b"\r")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Tab) => {
                    self.terminal_mut().write_bytes(b"\t")?;
                    return Ok(());
                }
                (KeyModifiers::SHIFT, KeyCode::Tab | KeyCode::BackTab) => {
                    self.terminal_mut().write_bytes(b"\x1b[Z")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Home) => {
                    self.write_terminal_cursor_key(b"\x1b[H", b"\x1bOH")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::End) => {
                    self.write_terminal_cursor_key(b"\x1b[F", b"\x1bOF")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Insert) => {
                    self.terminal_mut().write_bytes(b"\x1b[2~")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Delete) => {
                    self.terminal_mut().write_bytes(b"\x1b[3~")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.terminal_mut().write_bytes(b"\x1b")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::F(number)) if (1..=12).contains(&number) => {
                    self.terminal_mut().write_bytes(vt_function_key(number))?;
                    return Ok(());
                }
                (modifiers, KeyCode::Char(character))
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(byte) = terminal_control_byte(character) {
                        self.terminal_mut().write_bytes(&[byte])?;
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(c)) => self.terminal_mut().push_char(c),
            (KeyModifiers::NONE, KeyCode::Backspace) => self.terminal_mut().pop_char(),
            (KeyModifiers::NONE, KeyCode::Enter) => self.terminal_mut().submit()?,
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => self.terminal_mut().clear_output(),
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.terminal_mut().write_bytes(b"\x03")?
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.terminal_mut().write_bytes(b"\x04")?
            }
            (KeyModifiers::NONE, KeyCode::Tab) => self.terminal_mut().write_bytes(b"\t")?,
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.write_terminal_cursor_key(b"\x1b[A", b"\x1bOA")?
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.write_terminal_cursor_key(b"\x1b[B", b"\x1bOB")?
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.write_terminal_cursor_key(b"\x1b[C", b"\x1bOC")?
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.write_terminal_cursor_key(b"\x1b[D", b"\x1bOD")?
            }
            _ => self.handle_navigation(key),
        }
        Ok(())
    }
    fn handle_agent_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.agent_install_confirm {
            return self.handle_agent_install_key(key);
        }
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('t') {
            self.toggle_terminal_fullscreen();
            return Ok(());
        }
        if let Some(bytes) = terminal_modified_key(key) {
            self.agent.write_bytes(&bytes)?;
            return Ok(());
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.write_agent_cursor_key(b"\x1b[H", b"\x1bOH")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.write_agent_cursor_key(b"\x1b[F", b"\x1bOF")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Insert) => {
                self.agent.write_bytes(b"\x1b[2~")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                self.agent.write_bytes(b"\x1b[3~")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.agent.write_bytes(b"\x1b")?;
                return Ok(());
            }
            (KeyModifiers::NONE, KeyCode::F(number)) if (1..=12).contains(&number) => {
                self.agent.write_bytes(vt_function_key(number))?;
                return Ok(());
            }
            _ => {}
        }
        if self.agent.alternate_screen() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char(character)) => {
                    let mut bytes = [0_u8; 4];
                    self.agent
                        .write_bytes(character.encode_utf8(&mut bytes).as_bytes())?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Backspace) => {
                    self.agent.write_bytes(b"\x7f")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    self.agent.write_bytes(b"\r")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Tab) => {
                    self.agent.write_bytes(b"\t")?;
                    return Ok(());
                }
                (KeyModifiers::SHIFT, KeyCode::Tab | KeyCode::BackTab) => {
                    self.agent.write_bytes(b"\x1b[Z")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    self.agent.write_bytes(b"\x1b[5~")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    self.agent.write_bytes(b"\x1b[6~")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Home) => {
                    self.write_agent_cursor_key(b"\x1b[H", b"\x1bOH")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::End) => {
                    self.write_agent_cursor_key(b"\x1b[F", b"\x1bOF")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Insert) => {
                    self.agent.write_bytes(b"\x1b[2~")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Delete) => {
                    self.agent.write_bytes(b"\x1b[3~")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.agent.write_bytes(b"\x1b")?;
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::F(number)) if (1..=12).contains(&number) => {
                    self.agent.write_bytes(vt_function_key(number))?;
                    return Ok(());
                }
                (modifiers, KeyCode::Char(character))
                    if modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(byte) = terminal_control_byte(character) {
                        self.agent.write_bytes(&[byte])?;
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('[')) => self.cycle_agent(-1),
            (KeyModifiers::NONE, KeyCode::Char(']')) => self.cycle_agent(1),
            (KeyModifiers::NONE, KeyCode::Char(c)) if c != 'i' => self.agent.push_char(c),
            (KeyModifiers::NONE, KeyCode::Backspace) => self.agent.pop_char(),
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.agent_configured() && self.agent_executable_available() {
                    self.agent.submit()?;
                } else if self.agent_install_command().is_some() {
                    self.agent_install_confirm = true;
                    self.status_message =
                        "Agent is unavailable · Enter to install · Esc cancel".to_owned();
                } else {
                    self.status_message = if self.agent_configured() {
                        "Agent executable not found in PATH".to_owned()
                    } else {
                        "Configure agent.command in settings.json".to_owned()
                    };
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('i'))
                if !self.agent_executable_available() && self.agent_install_command().is_some() =>
            {
                self.agent_install_confirm = true;
                self.status_message =
                    "Review install command · Enter to run · Esc cancel".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('i')) => self.agent.push_char('i'),
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.agent.write_bytes(b"\x03")?,
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => self.agent.write_bytes(b"\x04")?,
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.write_agent_cursor_key(b"\x1b[A", b"\x1bOA")?
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.write_agent_cursor_key(b"\x1b[B", b"\x1bOB")?
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.write_agent_cursor_key(b"\x1b[C", b"\x1bOC")?
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.write_agent_cursor_key(b"\x1b[D", b"\x1bOD")?
            }
            _ => self.handle_navigation(key),
        }
        Ok(())
    }

    fn handle_agent_install_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.agent_install_confirm = false;
                self.status_message = "Agent installation cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let Some(command) = self.agent_install_command().map(str::to_owned) else {
                    self.agent_install_confirm = false;
                    return Ok(());
                };
                self.agent_install_confirm = false;
                self.agent_install_pending = true;
                for character in command.chars() {
                    self.terminal_mut().push_char(character);
                }
                self.terminal_mut().submit()?;
                self.bottom_visible = true;
                self.status_message = "Agent installation started in Terminal".to_owned();
            }
            _ => {}
        }
        Ok(())
    }
    fn restart_agent_session(&mut self) {
        self.agent_tool_generation.fetch_add(1, Ordering::AcqRel);
        self.agent_protocol_buffer.clear();
        let (command, cwd, _) = configured_agent_command(&self.settings, &self.agent_provider);
        self.agent = TerminalState::with_command(command, cwd);
    }
    fn cycle_agent(&mut self, delta: isize) {
        let providers: Vec<_> = self.settings.agent.providers.keys().cloned().collect();
        if providers.is_empty() {
            self.status_message = "No agent providers configured".to_owned();
            return;
        }
        let current = providers
            .iter()
            .position(|provider| provider == &self.agent_provider)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(providers.len() as isize) as usize;
        self.agent_provider = providers[next].clone();
        self.agent_install_pending = false;
        self.restart_agent_session();
        self.status_message = format!("Agent provider: {}", self.agent_name());
    }
    fn handle_search_key(&mut self, key: KeyEvent) {
        let refresh = match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.search_field = (self.search_field + 1) % 3;
                self.search_cursor = match self.search_field {
                    0 => self.search_query.chars().count(),
                    1 => self.search_include.chars().count(),
                    _ => self.search_exclude.chars().count(),
                };
                self.sync_search_cursor();
                self.status_message = format!(
                    "Search field: {}",
                    ["query", "include", "exclude"][self.search_field as usize]
                );
                false
            }
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::SHIFT, KeyCode::Tab) => {
                self.search_field = (self.search_field + 2) % 3;
                self.search_cursor = match self.search_field {
                    0 => self.search_query.chars().count(),
                    1 => self.search_include.chars().count(),
                    _ => self.search_exclude.chars().count(),
                };
                self.sync_search_cursor();
                self.status_message = format!(
                    "Search field: {}",
                    ["query", "include", "exclude"][self.search_field as usize]
                );
                false
            }
            (modifiers, KeyCode::Char('r'))
                if modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.search_regex = !self.search_regex;
                self.refresh_search();
                self.status_message = if self.search_regex {
                    "Search regex enabled"
                } else {
                    "Search smart-case enabled"
                }
                .to_owned();
                false
            }
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                self.move_search_result(1);
                false
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.move_search_result(-1);
                false
            }
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                if self.search_field != 0 {
                    self.search_field = 0;
                    self.search_cursor = self.search_query.chars().count();
                    self.status_message = "Search field: query".to_owned();
                } else {
                    self.replace_mode = !self.replace_mode;
                    self.sync_search_cursor();
                }
                false
            }
            (KeyModifiers::NONE, KeyCode::Char(c)) => self.edit_search_input("Char", Some(c)),
            (KeyModifiers::NONE, KeyCode::Backspace) => self.edit_search_input("Backspace", None),
            (KeyModifiers::NONE, KeyCode::Delete) => self.edit_search_input("Delete", None),
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.edit_search_input("Left", None);
                false
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.edit_search_input("Right", None);
                false
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.edit_search_input("Home", None);
                false
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.edit_search_input("End", None);
                false
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if self.search_field == 0 && !self.search_results.is_empty() {
                    self.search_index = (self.search_index + 1).min(self.search_results.len() - 1);
                }
                false
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                if self.search_field == 0 {
                    self.search_index = self.search_index.saturating_sub(1);
                }
                false
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.focus = Focus::Editor;
                false
            }
            (modifiers, KeyCode::Enter)
                if self.replace_mode
                    && modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Err(error) = self.replace_workspace_all() {
                    self.status_message = format!("Workspace replace failed: {error}");
                }
                false
            }
            (KeyModifiers::CONTROL, KeyCode::Enter) if self.replace_mode => {
                self.replace_current_match();
                false
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.search_field != 0 {
                    self.search_field = 0;
                    self.search_cursor = self.search_query.chars().count();
                    self.status_message = "Search field: query".to_owned();
                    false
                } else if self.replace_mode {
                    let query = self.search_query.clone();
                    let replacement = self.replace_query.clone();
                    let count = self.buffer_mut().replace_all(&query, &replacement);
                    self.sync_lsp_for_buffer();
                    self.status_message = format!("Replaced {count} occurrence(s)");
                    false
                } else {
                    if let Some(result) = self.search_results.get(self.search_index).cloned() {
                        let target_line = result.line.saturating_sub(1);
                        let target_column = result.column.saturating_sub(1);
                        if let Some(index) = self
                            .buffers
                            .iter()
                            .position(|buffer| buffer.path() == Some(result.path.as_path()))
                        {
                            self.active_tab = index;
                            self.buffer_mut().set_cursor(target_line, target_column);
                            self.selection_anchor = None;
                            self.sync_lsp_for_buffer();
                            self.refresh_search_results();
                            self.emit_plugin_event("tab.activate");
                            self.focus = Focus::Editor;
                            self.status_message = format!(
                                "Opened {}:{}:{}",
                                result.path.display(),
                                result.line,
                                result.column
                            );
                        } else {
                            match Buffer::open(&result.path) {
                                Ok(mut buffer) => {
                                    buffer.set_cursor(target_line, target_column);
                                    self.open_buffer(buffer);
                                    self.sync_lsp_for_buffer();
                                    self.emit_plugin_event("document.open");
                                    self.focus = Focus::Editor;
                                    self.status_message = format!(
                                        "Opened {}:{}:{}",
                                        result.path.display(),
                                        result.line,
                                        result.column
                                    );
                                }
                                Err(error) => self.status_message = format!("Open failed: {error}"),
                            }
                        }
                    } else {
                        self.status_message = format!("{} matches", self.search_results.len());
                    }
                    false
                }
            }
            _ => {
                self.handle_navigation(key);
                false
            }
        };
        if refresh {
            self.request_search_refresh();
        }
    }
    fn replace_workspace_all(&mut self) -> AppResult<()> {
        let query = self.search_query.clone();
        let replacement = self.replace_query.clone();
        if query.is_empty() {
            self.status_message = "Search query is empty".to_owned();
            return Ok(());
        }

        let active_path = self.buffer().path().map(Path::to_path_buf);
        let mut paths = Vec::new();
        for result in &self.search_results {
            if !paths.contains(&result.path) {
                paths.push(result.path.clone());
            }
        }
        if let Some(path) = &active_path {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }

        let mut replaced = 0;
        for path in paths {
            // Open buffers are the source of truth for workspace operations. In
            // particular, never replace a non-active dirty buffer from disk: doing
            // so would silently discard edits that are still visible in a tab.
            let open_index = self.buffers.iter().position(|buffer| {
                let Some(open_path) = buffer.path() else {
                    return false;
                };
                let canonical_open = open_path
                    .canonicalize()
                    .unwrap_or_else(|_| open_path.to_path_buf());
                let canonical_target = path.canonicalize().unwrap_or_else(|_| path.clone());
                canonical_open == canonical_target
            });
            if let Some(index) = open_index {
                if self.search_regex {
                    let content = self.buffers[index].lines().join("\n");
                    let (updated, count) =
                        replace_with_options(&content, &query, &replacement, true);
                    if count > 0 {
                        let last_line = self.buffers[index].lines().len().saturating_sub(1);
                        let last_col = self.buffers[index].lines()[last_line].chars().count();
                        self.buffers[index].replace_ranges(
                            &[(
                                Position { line: 0, column: 0 },
                                Position {
                                    line: last_line,
                                    column: last_col,
                                },
                            )],
                            &updated,
                        );
                    }
                    replaced += count;
                } else {
                    replaced += self.buffers[index].replace_all_smart_case(&query, &replacement);
                }
                continue;
            }
            let content = fs::read_to_string(&path)?;
            let (updated, count) =
                replace_with_options(&content, &query, &replacement, self.search_regex);
            if count > 0 {
                fs::write(&path, updated)?;
                replaced += count;
            }
        }
        self.sync_lsp_for_buffer();
        self.refresh_search();
        self.status_message = format!("Replaced {replaced} occurrence(s) across workspace");
        Ok(())
    }

    fn replace_current_match(&mut self) {
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            self.status_message = "Replace one requires a file-backed editor tab".to_owned();
            return;
        };
        let Some(selected) = self.search_results.get(self.search_index).cloned() else {
            self.status_message = "No search match selected".to_owned();
            return;
        };
        if selected.path != path {
            self.status_message = "Select a match in the active file before replacing".to_owned();
            return;
        }
        let content = self.file_lines().join("\n");
        let fresh_results =
            search_text_with_options(&path, &content, &self.search_query, self.search_regex);
        let selected_position = (selected.line, selected.column);
        let Some(fresh) = fresh_results
            .iter()
            .filter(|result| (result.line, result.column) >= selected_position)
            .min_by_key(|result| (result.line, result.column))
            .or_else(|| fresh_results.first())
        else {
            self.status_message = "Search match is no longer present".to_owned();
            return;
        };
        let line_index = fresh.line.saturating_sub(1);
        let Some(line) = self.file_lines().get(line_index).cloned() else {
            self.status_message = "Search match is no longer present".to_owned();
            return;
        };
        let (replacement, changed) = replace_first_with_options(
            &line,
            &self.search_query,
            &self.replace_query,
            self.search_regex,
            fresh.column.saturating_sub(1),
        );
        if !changed {
            self.status_message = "Search match changed before replacement".to_owned();
            return;
        }
        self.buffer_mut()
            .replace_line_range(line_index, line_index + 1, &[replacement]);
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
        self.status_message = format!("Replaced match at {}:{}", fresh.line, fresh.column);
    }
    fn refresh_search(&mut self) {
        self.refresh_search_results();
        self.status_message = if self.search_query.is_empty() {
            "Search workspace".to_owned()
        } else {
            format!("{} matches", self.search_results.len())
        };
    }
    fn refresh_search_results(&mut self) {
        let include = Self::parse_search_globs(&self.search_include);
        let exclude = Self::parse_search_globs(&self.search_exclude);
        self.search_results = search_workspace_roots_with_options(
            &self.workspace_roots,
            &self.search_query,
            &include,
            &exclude,
            self.search_regex,
        );
        self.merge_open_buffer_search_results();
        self.search_index = 0;
    }
    fn parse_search_globs(value: &str) -> Vec<String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }
    fn merge_open_buffer_search_results(&mut self) {
        let open_buffers = self
            .buffers
            .iter()
            .filter_map(|buffer| {
                buffer
                    .path()
                    .map(Path::to_path_buf)
                    .map(|path| (path, buffer.lines().join("\n")))
            })
            .collect::<Vec<_>>();
        for (path, text) in open_buffers {
            self.search_results.retain(|result| result.path != path);
            self.search_results.extend(search_text_with_options(
                &path,
                &text,
                &self.search_query,
                self.search_regex,
            ));
        }
    }
    fn move_search_result(&mut self, delta: isize) {
        if self.search_results.is_empty() {
            self.status_message = "No search results".to_owned();
            return;
        }
        self.search_index = (self.search_index as isize + delta)
            .rem_euclid(self.search_results.len() as isize) as usize;
        let result = &self.search_results[self.search_index];
        self.status_message = format!(
            "Match {}/{} · {}:{}:{}",
            self.search_index + 1,
            self.search_results.len(),
            result.path.display(),
            result.line,
            result.column
        );
    }
    fn request_markdown_image_paste(&mut self) -> bool {
        if !matches!(self.language(), "md" | "markdown") {
            return false;
        }
        if self.buffer().path().is_none() {
            return false;
        }
        if self.markdown_image_pending {
            self.status_message = "Checking clipboard image…".to_owned();
            return true;
        }
        let document = self.buffer().path().expect("checked above").to_path_buf();
        let sender = self.markdown_image_tx.clone();
        self.markdown_image_pending = true;
        self.status_message = "Checking clipboard image…".to_owned();
        std::thread::spawn(move || {
            let bytes = read_clipboard_png();
            let _ = sender.send(MarkdownImageCompletion { document, bytes });
        });
        true
    }

    fn insert_markdown_image_bytes(&mut self, bytes: &[u8]) -> AppResult<bool> {
        let Some(document) = self.buffer().path().map(Path::to_path_buf) else {
            return Ok(false);
        };
        const MAX_MARKDOWN_IMAGE_BYTES: usize = 32 * 1024 * 1024;
        if bytes.len() > MAX_MARKDOWN_IMAGE_BYTES {
            self.status_message = "Clipboard image exceeds 32 MiB limit".to_owned();
            return Ok(true);
        }
        let relative_dir = safe_markdown_image_dir(&self.settings.editor.markdown_image_dir)
            .unwrap_or_else(|| PathBuf::from(".mtide/assets"));
        let Some(parent) = document.parent() else {
            return Ok(false);
        };
        let target_dir = parent.join(&relative_dir);
        fs::create_dir_all(&target_dir)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut target = None;
        for index in 0..1000u32 {
            let suffix = if index == 0 {
                String::new()
            } else {
                format!("-{index}")
            };
            let name = format!("image-{stamp}-{}{suffix}.png", std::process::id());
            let candidate = target_dir.join(name);
            if !candidate.exists() {
                target = Some(candidate);
                break;
            }
        }
        let Some(target) = target else {
            return Err(crate::error::AppError::Workspace(
                "unable to allocate Markdown image filename".to_owned(),
            ));
        };
        let temporary = target.with_extension("png.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, &target)?;
        let link = relative_dir
            .join(target.file_name().expect("generated image filename"))
            .to_string_lossy()
            .replace('\\', "/");
        self.handle_paste(&format!("![image]({link})"))?;
        self.status_message = format!("Inserted Markdown image {link}");
        Ok(true)
    }

    fn handle_editor_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.plugin_file_view.is_some() {
            self.handle_plugin_file_key(key);
            return Ok(());
        }
        if self.rename_mode {
            return self.handle_rename_key(key);
        }
        let initial_cursor = self.cursor();
        let key = match (key.modifiers, key.code) {
            (KeyModifiers::SHIFT, KeyCode::Insert) => {
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
            }
            (KeyModifiers::SHIFT, KeyCode::Delete) => {
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
            }
            (KeyModifiers::CONTROL, KeyCode::Insert) => {
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            }
            _ => key,
        };
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.close_active_tab();
                return Ok(());
            }
            (KeyModifiers::CONTROL | KeyModifiers::SHIFT, KeyCode::Char('t' | 'T')) => {
                self.reopen_closed_tab();
                return Ok(());
            }
            (KeyModifiers::ALT, KeyCode::Left) => {
                self.switch_tab(-1);
                return Ok(());
            }
            (KeyModifiers::ALT, KeyCode::Right) => {
                self.switch_tab(1);
                return Ok(());
            }
            _ => {}
        }
        if self.handle_lsp_popup_key(key)? {
            return Ok(());
        }
        if key.modifiers == KeyModifiers::ALT {
            match key.code {
                KeyCode::Up => {
                    self.add_secondary_cursor(-1);
                    return Ok(());
                }
                KeyCode::Down => {
                    self.add_secondary_cursor(1);
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('n') => {
                    self.select_next_occurrence();
                    return Ok(());
                }
                KeyCode::Char('b') => {
                    self.resolve_current_conflict_base();
                    return Ok(());
                }
                KeyCode::Char('k') => {
                    self.resolve_current_conflict_both();
                    return Ok(());
                }
                KeyCode::Char('p') => {
                    self.open_conflict_preview();
                    return Ok(());
                }
                KeyCode::Up => {
                    self.jump_conflict(false);
                    return Ok(());
                }
                KeyCode::Down => {
                    self.jump_conflict(true);
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.reload_active_buffer()?;
                    return Ok(());
                }
                KeyCode::Char('l') => {
                    self.cycle_language_override();
                    return Ok(());
                }
                KeyCode::Char('m') => {
                    self.goto_matching_bracket();
                    return Ok(());
                }
                KeyCode::Left => {
                    self.scroll_editor_horizontal(-4);
                    return Ok(());
                }
                KeyCode::Right => {
                    self.scroll_editor_horizontal(4);
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers == (KeyModifiers::ALT | KeyModifiers::SHIFT) {
            match key.code {
                KeyCode::Up => {
                    if self.selection_anchor.is_some() || !self.secondary_selections.is_empty() {
                        self.move_selected_lines(true);
                        self.emit_plugin_event("editor.change");
                    } else {
                        self.add_secondary_cursor(-1);
                        self.emit_plugin_event("cursor.change");
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if self.selection_anchor.is_some() || !self.secondary_selections.is_empty() {
                        self.move_selected_lines(false);
                        self.emit_plugin_event("editor.change");
                    } else {
                        self.add_secondary_cursor(1);
                        self.emit_plugin_event("cursor.change");
                    }
                    return Ok(());
                }
                KeyCode::Char('s') => {
                    self.sort_selected_lines(false);
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                KeyCode::Char('u') => {
                    self.transform_selected_lines(true);
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                KeyCode::Char('l') => {
                    self.transform_selected_lines(false);
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                KeyCode::Char('i') => {
                    self.split_selection_into_lines();
                    self.emit_plugin_event("cursor.change");
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(
                key.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            )
        {
            self.selection_anchor.get_or_insert(self.buffer().cursor());
            match key.code {
                KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.buffer_mut().move_word_left()
                }
                KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.buffer_mut().move_word_right()
                }
                KeyCode::Left => self.buffer_mut().move_cursor(0, -1),
                KeyCode::Right => self.buffer_mut().move_cursor(0, 1),
                KeyCode::Up => self.move_cursor_vertically(-1),
                KeyCode::Down => self.move_cursor_vertically(1),
                KeyCode::PageUp => self.move_cursor_by_page(-1),
                KeyCode::PageDown => self.move_cursor_by_page(1),
                KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.buffer_mut().move_file_start()
                }
                KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.buffer_mut().move_file_end()
                }
                KeyCode::Home => self.buffer_mut().move_home(),
                KeyCode::End => self.buffer_mut().move_end(),
                _ => {}
            }
            self.emit_plugin_event("cursor.change");
            return Ok(());
        }
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Backspace => {
                    if self.selection_anchor.is_some() || !self.secondary_selections.is_empty() {
                        self.replace_active_selections("");
                    } else if self.secondary_cursors.is_empty() {
                        self.buffer_mut().delete_word_backward();
                    }
                }
                KeyCode::Delete => {
                    if self.selection_anchor.is_some() || !self.secondary_selections.is_empty() {
                        self.replace_active_selections("");
                    } else if self.secondary_cursors.is_empty() {
                        self.buffer_mut().delete_word_forward();
                    }
                }
                KeyCode::Enter => self.insert_line_below(),
                KeyCode::Char('o') => {
                    self.open_quick_open();
                    return Ok(());
                }
                KeyCode::Char('f') => {
                    self.open_search_from_editor();
                    return Ok(());
                }
                KeyCode::Char('g') => {
                    self.open_goto();
                    return Ok(());
                }
                KeyCode::Char('j') => {
                    self.join_selected_lines();
                    return Ok(());
                }
                KeyCode::Char('a') => {
                    self.select_all();
                    self.emit_plugin_event("cursor.change");
                    return Ok(());
                }
                KeyCode::Char('c') => {
                    if !self.secondary_selections.is_empty() {
                        self.clipboard = self
                            .active_selection_ranges()
                            .iter()
                            .map(|(first, second)| self.buffer().text_between(*first, *second))
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.queue_osc52_clipboard();
                        self.status_message =
                            format!("Copied {} characters", self.clipboard.chars().count());
                        return Ok(());
                    }
                    if !self.secondary_cursors.is_empty() {
                        self.clipboard = self
                            .cursor_positions()
                            .iter()
                            .filter_map(|position| self.file_lines().get(position.line))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.queue_osc52_clipboard();
                        self.status_message =
                            format!("Copied {} characters", self.clipboard.chars().count());
                        return Ok(());
                    }
                    if let Some(anchor) = self.selection_anchor {
                        self.clipboard = self.buffer().text_between(anchor, self.buffer().cursor());
                        self.queue_osc52_clipboard();
                        self.status_message =
                            format!("Copied {} characters", self.clipboard.chars().count());
                        return Ok(());
                    }
                }
                KeyCode::Char('x') => {
                    if !self.secondary_selections.is_empty() {
                        self.clipboard = self
                            .active_selection_ranges()
                            .iter()
                            .map(|(first, second)| self.buffer().text_between(*first, *second))
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.queue_osc52_clipboard();
                        self.replace_active_selections("");
                        self.status_message = "Cut selections".to_owned();
                        self.emit_plugin_event("editor.change");
                        return Ok(());
                    }
                    if let Some(anchor) = self.selection_anchor.take() {
                        let cursor = self.buffer().cursor();
                        self.clipboard = self.buffer().text_between(anchor, cursor);
                        self.queue_osc52_clipboard();
                        self.buffer_mut().delete_between(anchor, cursor);
                        self.status_message = "Cut selection".to_owned();
                        self.sync_lsp_for_buffer();
                        self.refresh_search_results();
                        self.emit_plugin_event("editor.change");
                        return Ok(());
                    }
                }
                KeyCode::Char('v') => {
                    if self.clipboard.is_empty() && self.request_markdown_image_paste() {
                        return Ok(());
                    }
                    if self.clipboard.is_empty() {
                        return Ok(());
                    }
                    if !self.secondary_selections.is_empty() {
                        let clipboard = self.clipboard.clone();
                        self.paste_at_selections(&clipboard);
                        self.emit_plugin_event("editor.change");
                        return Ok(());
                    }
                    if !self.secondary_cursors.is_empty() {
                        let clipboard = self.clipboard.clone();
                        self.paste_at_cursors(&clipboard);
                        self.emit_plugin_event("editor.change");
                        return Ok(());
                    }
                    if let Some(anchor) = self.selection_anchor.take() {
                        let cursor = self.buffer().cursor();
                        self.buffer_mut().delete_between(anchor, cursor);
                    }
                    let clipboard = self.clipboard.clone();
                    self.buffer_mut().insert(&clipboard);
                    self.sync_lsp_for_buffer();
                    self.refresh_search_results();
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers == KeyModifiers::ALT {
            match key.code {
                KeyCode::Backspace => {
                    if self.selection_anchor.is_some() || !self.secondary_selections.is_empty() {
                        self.replace_active_selections("");
                    } else if self.secondary_cursors.is_empty() {
                        self.buffer_mut().delete_word_backward();
                    }
                }
                KeyCode::Delete => {
                    if self.selection_anchor.is_some() || !self.secondary_selections.is_empty() {
                        self.replace_active_selections("");
                    } else if self.secondary_cursors.is_empty() {
                        self.buffer_mut().delete_word_forward();
                    }
                }
                _ => {}
            }
        }
        if !self.secondary_selections.is_empty() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char(character)) => {
                    self.replace_active_selections(&character.to_string());
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let newline = self.newline_with_indent();
                    self.replace_active_selections(&newline);
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Backspace | KeyCode::Delete) => {
                    self.replace_active_selections("");
                    self.emit_plugin_event("editor.change");
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            match key.code {
                KeyCode::Char('o') => self.request_lsp("textDocument/documentSymbol")?,
                KeyCode::Char('[') => {
                    self.toggle_fold();
                    return Ok(());
                }
                KeyCode::Char(']') => {
                    self.folded_lines.clear();
                    self.status_message = "Expanded all folds".to_owned();
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.selection_anchor.is_some()
            && matches!(
                (key.modifiers, key.code),
                (KeyModifiers::NONE, KeyCode::Char(_))
                    | (KeyModifiers::NONE, KeyCode::Enter)
                    | (KeyModifiers::NONE, KeyCode::Backspace | KeyCode::Delete)
            )
        {
            if let Some(anchor) = self.selection_anchor.take() {
                let cursor = self.buffer().cursor();
                self.buffer_mut().delete_between(anchor, cursor);
            }
        }
        if self.selection_anchor.is_some()
            && matches!(
                (key.modifiers, key.code),
                (KeyModifiers::NONE, KeyCode::Tab)
                    | (KeyModifiers::SHIFT, KeyCode::Tab | KeyCode::BackTab)
            )
        {
            self.indent_selected_lines(key.modifiers.contains(KeyModifiers::SHIFT));
            self.sync_lsp_for_buffer();
            self.refresh_search_results();
            self.emit_plugin_event("editor.change");
            return Ok(());
        }
        let modifies_document = matches!(
            (key.modifiers, key.code),
            (KeyModifiers::CONTROL, KeyCode::Char('s' | 'z' | 'y'))
                | (KeyModifiers::CONTROL, KeyCode::Backspace | KeyCode::Delete)
                | (KeyModifiers::CONTROL, KeyCode::Enter)
                | (KeyModifiers::ALT, KeyCode::Backspace | KeyCode::Delete)
                | (KeyModifiers::NONE, KeyCode::Char(_))
                | (KeyModifiers::NONE, KeyCode::Enter)
                | (KeyModifiers::NONE, KeyCode::Backspace)
                | (KeyModifiers::NONE, KeyCode::Delete)
        ) || (key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('k')))
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('/'));
        let signature_trigger = matches!(
            (key.modifiers, key.code),
            (KeyModifiers::NONE, KeyCode::Char('(' | ','))
        );
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char(' ')) => {
                if self.settings.lsp.enabled
                    && !self.language().is_empty()
                    && self.lsp.configured(self.language())
                {
                    self.request_lsp("textDocument/completion")?
                } else {
                    self.request_ai_completion();
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                self.request_lsp("textDocument/hover")?
            }
            (modifiers, KeyCode::Char('s'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::ALT) =>
            {
                self.request_lsp("textDocument/signatureHelp")?
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.request_lsp("textDocument/definition")?
            }
            (modifiers, KeyCode::Char('o'))
                if modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.resolve_current_conflict(true)
            }
            (modifiers, KeyCode::Char('e'))
                if modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.resolve_current_conflict(false)
            }
            (modifiers, KeyCode::Char('r'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::ALT) =>
            {
                self.request_lsp("textDocument/references")?
            }
            (modifiers, KeyCode::Char('i'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::ALT) =>
            {
                self.request_lsp("textDocument/implementation")?
            }
            (modifiers, KeyCode::Char('t'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::ALT) =>
            {
                self.request_lsp("textDocument/typeDefinition")?
            }
            (modifiers, KeyCode::Char('s'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.open_save_as();
                return Ok(());
            }
            (modifiers, KeyCode::Char('f'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.request_lsp_edits("textDocument/formatting")?
            }
            (modifiers, KeyCode::Char('a'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.request_lsp_edits("textDocument/codeAction")?
            }
            (modifiers, KeyCode::Char('r'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.rename_mode = true;
                self.rename_text.clear();
                self.status_message = "Rename symbol · Enter apply · Esc cancel".to_owned();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
                let editor = self.settings.editor.clone();
                if editor.format_on_save {
                    if self.formatter_configured() {
                        if let Err(error) = self.run_external_formatter() {
                            self.status_message = format!("Format-on-save skipped: {error}");
                        }
                    } else {
                        let pending_before = self.lsp_pending.len();
                        if let Err(error) = self.request_lsp_edits("textDocument/formatting") {
                            self.status_message = format!("Format-on-save skipped: {error}");
                        }
                        if self.lsp_pending.len() > pending_before {
                            self.format_on_save_pending = true;
                            self.status_message = "Formatting before save…".to_owned();
                            return Ok(());
                        }
                    }
                }
                self.save_active_buffer_now(&editor)?;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('z')) => self.buffer_mut().undo(),
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => self.buffer_mut().redo(),
            (modifiers, KeyCode::Char('d'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.buffer_mut().duplicate_line()
            }
            (modifiers, KeyCode::Char('k'))
                if modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.buffer_mut().delete_line()
            }
            (modifiers, KeyCode::Char('/')) if modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(marker) = self.comment_marker() {
                    self.buffer_mut().toggle_comment(marker)
                } else {
                    self.status_message = "Comment syntax unavailable for this language".to_owned();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char(c)) => self.insert_editor_character(c),
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.secondary_cursors.is_empty() {
                    let newline = self.newline_with_indent();
                    self.buffer_mut().insert(&newline);
                } else {
                    self.apply_multi_cursor_insert("\n");
                }
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if self.secondary_cursors.is_empty() {
                    self.buffer_mut().backspace();
                } else {
                    let positions = self.cursor_positions();
                    let result = self.buffer_mut().backspace_at_positions(&positions);
                    self.set_multi_cursor_positions(result);
                }
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                if self.secondary_cursors.is_empty() {
                    self.buffer_mut().delete();
                } else {
                    let positions = self.cursor_positions();
                    let result = self.buffer_mut().delete_at_positions(&positions);
                    self.set_multi_cursor_positions(result);
                }
            }
            (KeyModifiers::NONE, KeyCode::Left) => self.buffer_mut().move_cursor(0, -1),
            (KeyModifiers::NONE, KeyCode::Right) => self.buffer_mut().move_cursor(0, 1),
            (modifiers, KeyCode::Left) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.buffer_mut().move_word_left()
            }
            (modifiers, KeyCode::Right) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.buffer_mut().move_word_right()
            }
            (KeyModifiers::NONE, KeyCode::Up) => self.move_cursor_vertically(-1),
            (KeyModifiers::NONE, KeyCode::Down) => self.move_cursor_vertically(1),
            (KeyModifiers::NONE, KeyCode::PageUp) => self.move_cursor_by_page(-1),
            (KeyModifiers::NONE, KeyCode::PageDown) => self.move_cursor_by_page(1),
            (KeyModifiers::CONTROL, KeyCode::Home) => self.buffer_mut().move_file_start(),
            (KeyModifiers::CONTROL, KeyCode::End) => self.buffer_mut().move_file_end(),
            (KeyModifiers::NONE, KeyCode::Home) => self.buffer_mut().move_home(),
            (KeyModifiers::NONE, KeyCode::End) => self.buffer_mut().move_end(),
            _ => self.handle_navigation(key),
        }
        if modifies_document {
            self.ai_generation.fetch_add(1, Ordering::AcqRel);
            if self.lsp_popup.is_some() {
                self.close_lsp_popup();
            }
            self.sync_lsp_for_buffer();
            if signature_trigger
                && self.settings.lsp.enabled
                && self.lsp.configured(self.language())
                && self.lsp_popup.is_none()
            {
                if let Err(error) = self.request_lsp("textDocument/signatureHelp") {
                    self.status_message = format!("Signature help unavailable: {error}");
                }
                self.completion_due = None;
            } else if matches!(
                (key.modifiers, key.code),
                (KeyModifiers::NONE, KeyCode::Char(_))
                    | (KeyModifiers::NONE, KeyCode::Backspace | KeyCode::Delete)
            ) && !self.language().is_empty()
                && ((self.settings.lsp.enabled && self.lsp.configured(self.language()))
                    || (self.settings.ai.enabled && self.settings.ai.auto_suggest))
            {
                self.completion_due = Some(Instant::now() + Duration::from_millis(150));
            }
            self.refresh_search_results();
            self.emit_plugin_event("editor.change");
        } else if self.cursor() != initial_cursor {
            self.emit_plugin_event("cursor.change");
        }
        if !key.modifiers.contains(KeyModifiers::SHIFT) {
            self.selection_anchor = None;
        }
        Ok(())
    }

    fn activate_plugin_tab_selection(&mut self) {
        let Some((runtime_index, id)) = self.plugin_custom_tab.clone() else {
            return;
        };
        let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
            return;
        };
        let panel_id = format!("__tab:{id}");
        match runtime.activate_panel_selection(&panel_id, self.plugin_tab_scroll) {
            Ok(true) => {
                self.apply_plugin_editor_requests();
                self.refresh_plugin_custom_tab();
            }
            Ok(false) => {}
            Err(error) => {
                self.status_message = format!("Plugin selection failed: {error}");
            }
        }
    }

    fn refresh_plugin_file_view(&mut self) -> AppResult<()> {
        let Some((runtime_index, file_id)) = self.plugin_file_close else {
            return Ok(());
        };
        let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
            return Ok(());
        };
        if let Some(text) = runtime.render_file_view(file_id)? {
            self.plugin_tab_scroll = self
                .plugin_tab_scroll
                .min(text.lines().count().saturating_sub(1));
            if let Some((_, current_text)) = self.plugin_file_view.as_mut() {
                *current_text = text;
            }
        }
        Ok(())
    }

    fn dispatch_plugin_view_key(&mut self, key: KeyEvent) -> AppResult<bool> {
        let (runtime_index, panel_id) =
            if let Some((runtime_index, id)) = self.plugin_custom_tab.as_ref() {
                (*runtime_index, format!("__tab:{id}"))
            } else if let Some((runtime_index, file_id)) = self.plugin_file_close {
                (runtime_index, format!("__file:{file_id}"))
            } else {
                return Ok(false);
            };
        let Some(runtime) = self.plugin_runtimes.get(runtime_index) else {
            return Ok(false);
        };
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let backwards =
                key.modifiers.contains(KeyModifiers::SHIFT) || matches!(key.code, KeyCode::BackTab);
            if runtime.cycle_panel_focus(&panel_id, backwards) {
                self.refresh_plugin_custom_tab();
                self.refresh_plugin_file_view()?;
                return Ok(true);
            }
        }
        let key_name = key_chord(key).unwrap_or_else(|| format!("{:?}", key.code));
        let rune = match key.code {
            KeyCode::Char(character) => Some(character),
            _ => None,
        };
        let modifier = format_key_modifier(key.modifiers);
        let mut handled = runtime.panel_input_key(&panel_id, &key_name, rune, &modifier)?
            || runtime.panel_password_key(&panel_id, &key_name, rune, &modifier)?
            || runtime.panel_checkbox_key(&panel_id, &key_name, rune)?
            || runtime.panel_dropdown_key(&panel_id, &key_name)?
            || runtime.panel_radio_key(&panel_id, &key_name)?
            || runtime.panel_tabs_key(&panel_id, &key_name)?
            || runtime.panel_scrollview_key(&panel_id, &key_name)?
            || runtime.panel_button_key(&panel_id, &key_name, rune)?;
        if !handled {
            handled = runtime.emit_panel_key_event(&panel_id, &key_name, rune, &modifier)?;
        }
        if !handled && key.modifiers.is_empty() {
            if let KeyCode::Char(character) = key.code {
                handled = runtime.activate_panel_command(&panel_id, &character.to_string())?;
            }
        }
        if !handled {
            handled = match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => runtime.move_panel_selection(&panel_id, -1)?,
                (KeyModifiers::NONE, KeyCode::Down) => {
                    runtime.move_panel_selection(&panel_id, 1)?
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    runtime.move_panel_selection(&panel_id, -8)?
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    runtime.move_panel_selection(&panel_id, 8)?
                }
                (KeyModifiers::NONE, KeyCode::Left) => {
                    runtime.move_tree_horizontal_selection(&panel_id, false)?
                        || runtime.move_panel_horizontal_selection(&panel_id, -1)?
                }
                (KeyModifiers::NONE, KeyCode::Right) => {
                    runtime.move_tree_horizontal_selection(&panel_id, true)?
                        || runtime.move_panel_horizontal_selection(&panel_id, 1)?
                }
                (KeyModifiers::NONE, KeyCode::Home) => {
                    runtime.move_panel_selection(&panel_id, isize::MIN)?
                }
                (KeyModifiers::NONE, KeyCode::End) => {
                    runtime.move_panel_selection(&panel_id, isize::MAX)?
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    runtime.activate_panel_current_selection(&panel_id)?
                }
                _ => false,
            };
        }
        if handled {
            self.apply_plugin_editor_requests();
            self.apply_plugin_bookmark_events();
            self.refresh_plugin_custom_tab();
            self.refresh_plugin_file_view()?;
        }
        Ok(handled)
    }

    fn handle_plugin_file_key(&mut self, key: KeyEvent) {
        let Some((_, text)) = self.plugin_file_view.as_ref() else {
            return;
        };
        let line_count = text.lines().count().max(1);
        let hunk_positions: Vec<usize> = text
            .lines()
            .enumerate()
            .filter_map(|(index, line)| line.starts_with("@@").then_some(index))
            .collect();
        let reserved = matches!(
            (key.modifiers, key.code),
            (KeyModifiers::CONTROL, KeyCode::Char('w'))
                | (KeyModifiers::NONE, KeyCode::Esc)
                | (KeyModifiers::NONE, KeyCode::Up)
                | (KeyModifiers::NONE, KeyCode::Down)
                | (KeyModifiers::NONE, KeyCode::PageUp)
                | (KeyModifiers::NONE, KeyCode::PageDown)
                | (KeyModifiers::NONE, KeyCode::Home)
                | (KeyModifiers::NONE, KeyCode::End)
                | (KeyModifiers::NONE, KeyCode::Enter)
                | (KeyModifiers::NONE, KeyCode::Char('v'))
                | (KeyModifiers::NONE, KeyCode::Char('c'))
                | (KeyModifiers::NONE, KeyCode::Char('['))
                | (KeyModifiers::NONE, KeyCode::Char(']'))
        );
        match self.dispatch_plugin_view_key(key) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                self.status_message = format!("Plugin control failed: {error}");
                return;
            }
        }
        if (!reserved) && (self.plugin_custom_tab.is_some() || self.plugin_file_close.is_some()) {
            let key_name = key_chord(key).unwrap_or_else(|| format!("{:?}", key.code));
            let rune = match key.code {
                KeyCode::Char(character) => Some(character),
                _ => None,
            };
            let modifier = format_key_modifier(key.modifiers);
            let event_result = if let Some((runtime_index, id)) = self.plugin_custom_tab.clone() {
                self.plugin_runtimes
                    .get(runtime_index)
                    .map(|runtime| runtime.emit_tab_key_event(&id, &key_name, rune, &modifier))
            } else if let Some((runtime_index, close_id)) = self.plugin_file_close {
                self.plugin_runtimes.get(runtime_index).map(|runtime| {
                    runtime.emit_file_key_event(close_id, &key_name, rune, &modifier)
                })
            } else {
                None
            };
            if let Some(event_result) = event_result {
                match event_result {
                    Ok(true) => {
                        self.apply_plugin_editor_requests();
                        if self.plugin_custom_tab.is_some() {
                            self.refresh_plugin_custom_tab();
                        }
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.status_message = format!("Plugin tab event failed: {error}");
                        return;
                    }
                }
            }
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('w')) | (KeyModifiers::NONE, KeyCode::Esc) => {
                self.close_plugin_file_view();
                self.status_message = "Closed plugin tab".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.plugin_tab_scroll = self.plugin_tab_scroll.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.plugin_tab_scroll = (self.plugin_tab_scroll + 1).min(line_count - 1);
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.plugin_tab_scroll = self.plugin_tab_scroll.saturating_sub(8);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.plugin_tab_scroll = (self.plugin_tab_scroll + 8).min(line_count - 1);
            }
            (KeyModifiers::NONE, KeyCode::Home) => self.plugin_tab_scroll = 0,
            (KeyModifiers::NONE, KeyCode::End) => self.plugin_tab_scroll = line_count - 1,
            (KeyModifiers::NONE, KeyCode::Enter)
                if self.git_diff_tab_text.is_some() || self.plugin_diff_file_path.is_some() =>
            {
                let (path, base_lines) = if let Some(path) = self.plugin_diff_file_path.clone() {
                    let path = if path.is_absolute() {
                        path
                    } else {
                        self.search_root.join(path)
                    };
                    let base = path
                        .strip_prefix(&self.search_root)
                        .ok()
                        .and_then(|relative| relative.to_str())
                        .and_then(|relative| git_base_file(&self.search_root, relative));
                    (path, base)
                } else if let Some(change) = self.changes.get(self.change_index).cloned() {
                    let root = self.change_root(self.change_index).to_path_buf();
                    let path = root.join(&change.path);
                    let base = (change.index != '?' && change.worktree != '?')
                        .then(|| git_base_file(&root, &change.path))
                        .flatten();
                    (path, base)
                } else {
                    return;
                };
                if path.is_file() {
                    let Ok(mut buffer) = Buffer::open(&path) else {
                        self.status_message = format!("Unable to open {}", path.display());
                        return;
                    };
                    if let Some(line) = hunk_start_line(&self.change_diff, self.plugin_tab_hunk) {
                        buffer.set_cursor(line.saturating_sub(1), 0);
                    }
                    self.open_buffer(buffer);
                    self.diff_base_lines = base_lines;
                    self.close_plugin_file_view();
                    self.focus = Focus::Editor;
                    self.status_message = format!("Editing {} from Git diff", path.display());
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter)
                if self.plugin_custom_tab.as_ref().is_some_and(|_| {
                    self.git_diff_tab_text.is_none() && self.plugin_diff_file_path.is_none()
                }) =>
            {
                if let Some((runtime_index, id)) = self.plugin_custom_tab.clone() {
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        let panel_id = format!("__tab:{id}");
                        let activation =
                            runtime.activate_panel_row(&panel_id, self.plugin_tab_scroll);
                        let activation = match activation {
                            Ok(true) => Ok(true),
                            Ok(false) => {
                                runtime.activate_panel_selection(&panel_id, self.plugin_tab_scroll)
                            }
                            Err(error) => Err(error),
                        };
                        match activation {
                            Ok(true) => {
                                self.apply_plugin_editor_requests();
                                self.refresh_plugin_custom_tab();
                                self.status_message = "Plugin row activated".to_owned();
                            }
                            Ok(false) => {}
                            Err(error) => {
                                self.status_message = format!("Plugin row action failed: {error}");
                            }
                        }
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('v'))
                if self.plugin_custom_tab.is_some() || self.git_diff_tab_text.is_some() =>
            {
                self.plugin_tab_split = !self.plugin_tab_split;
                if self.plugin_custom_tab.is_some() {
                    self.refresh_plugin_custom_tab();
                } else {
                    self.refresh_git_diff_tab();
                }
                self.status_message = if self.plugin_tab_split {
                    "Diff tab: split view"
                } else {
                    "Diff tab: unified view"
                }
                .to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('c')) if self.plugin_custom_tab.is_some() => {
                if !self
                    .plugin_tab_collapsed_hunks
                    .remove(&self.plugin_tab_hunk)
                {
                    self.plugin_tab_collapsed_hunks.insert(self.plugin_tab_hunk);
                }
                self.refresh_plugin_custom_tab();
            }
            (KeyModifiers::NONE, KeyCode::Char('[')) if !hunk_positions.is_empty() => {
                self.plugin_tab_hunk = self.plugin_tab_hunk.saturating_sub(1);
                self.plugin_tab_scroll = hunk_positions[self.plugin_tab_hunk];
                self.status_message = format!(
                    "Plugin diff hunk {}/{}",
                    self.plugin_tab_hunk + 1,
                    hunk_positions.len()
                );
            }
            (KeyModifiers::NONE, KeyCode::Char(']')) if !hunk_positions.is_empty() => {
                self.plugin_tab_hunk =
                    (self.plugin_tab_hunk + 1).min(hunk_positions.len().saturating_sub(1));
                self.plugin_tab_scroll = hunk_positions[self.plugin_tab_hunk];
                self.status_message = format!(
                    "Plugin diff hunk {}/{}",
                    self.plugin_tab_hunk + 1,
                    hunk_positions.len()
                );
            }
            _ => {}
        }
        if self.plugin_custom_tab.is_some()
            && matches!(
                (key.modifiers, key.code),
                (
                    KeyModifiers::NONE,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Home
                        | KeyCode::End
                )
            )
        {
            self.activate_plugin_tab_selection();
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => self.rename_text.push(character),
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.rename_text.pop();
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.rename_mode = false;
                self.rename_text.clear();
                self.status_message = "Rename cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.rename_text.trim().is_empty() {
                    self.status_message = "Rename requires a non-empty name".to_owned();
                    return Ok(());
                }
                self.rename_mode = false;
                self.request_lsp_rename(&self.rename_text.clone())?;
                self.rename_text.clear();
            }
            _ => {}
        }
        Ok(())
    }
    fn insert_editor_character(&mut self, character: char) {
        if matches!(character, ')' | ']' | '}') {
            let cursor = self.cursor();
            let line = self.file_lines()[cursor.line].clone();
            let leading = line
                .chars()
                .take_while(|value| value.is_whitespace())
                .count();
            if cursor.column == leading
                && line.chars().skip(leading).all(char::is_whitespace)
                && leading > 0
            {
                let indent_width = self.effective_indent_size();
                self.buffer_mut()
                    .outdent_lines(cursor.line, cursor.line, indent_width);
            }
        }
        if !self.settings.editor.auto_closing_brackets {
            if self.secondary_cursors.is_empty() {
                self.buffer_mut().insert(&character.to_string());
            } else {
                self.apply_multi_cursor_insert_with_offset(&character.to_string(), 0);
            }
            return;
        }
        let closing = match character {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            '"' => Some('"'),
            '\'' => Some('\''),
            _ => None,
        };
        if self.secondary_cursors.is_empty() {
            if let Some(next) = self.buffer().char_at_cursor() {
                if next == character && matches!(character, ')' | ']' | '}' | '"' | '\'') {
                    self.buffer_mut().move_cursor(0, 1);
                    return;
                }
            }
            if let Some(closing) = closing {
                self.buffer_mut().insert(&format!("{character}{closing}"));
                self.buffer_mut().move_cursor(0, -1);
            } else {
                self.buffer_mut().insert(&character.to_string());
            }
            return;
        }
        let text = closing.map_or_else(
            || character.to_string(),
            |closing| format!("{character}{closing}"),
        );
        self.apply_multi_cursor_insert_with_offset(&text, usize::from(closing.is_some()));
    }
    fn indent_selected_lines(&mut self, outdent: bool) {
        let Some((start, end)) = self.selection_lines() else {
            return;
        };
        let prefix = if self.effective_use_tabs() {
            "\t".to_owned()
        } else {
            " ".repeat(self.effective_indent_size())
        };
        let prefix_width = prefix.chars().count();
        if outdent {
            self.buffer_mut().outdent_lines(start, end, prefix_width);
        } else {
            self.buffer_mut().indent_lines(start, end, &prefix);
        }
        if let Some(anchor) = self.selection_anchor.as_mut() {
            if (start..=end).contains(&anchor.line) {
                anchor.column = if outdent {
                    anchor.column.saturating_sub(prefix_width)
                } else {
                    anchor.column.saturating_add(prefix_width)
                };
            }
        }
        self.status_message = if outdent {
            "Outdented selected lines".to_owned()
        } else {
            "Indented selected lines".to_owned()
        };
    }
    fn newline_with_indent(&self) -> String {
        let cursor = self.cursor();
        let line = &self.file_lines()[cursor.line];
        let leading: String = line
            .chars()
            .take_while(|character| *character == ' ' || *character == '\t')
            .collect();
        let before_cursor: String = line.chars().take(cursor.column).collect();
        let trimmed = before_cursor.trim_end();
        let extra = if trimmed.ends_with('{') || trimmed.ends_with(':') {
            if self.effective_use_tabs() {
                "\t".to_owned()
            } else {
                " ".repeat(self.effective_indent_size())
            }
        } else {
            String::new()
        };
        format!("\n{leading}{extra}")
    }
    fn effective_indent_size(&self) -> usize {
        if self.auto_detect_indent {
            if let Some(size) = self.buffer().indent_info().size {
                return size;
            }
        }
        self.settings.editor.indent_size.max(1)
    }
    fn effective_use_tabs(&self) -> bool {
        if self.auto_detect_indent && self.buffer().indent_info().has_tabs {
            return self.buffer().indent_info().use_tabs;
        }
        self.settings.editor.use_tabs
    }
    fn apply_multi_cursor_insert(&mut self, text: &str) {
        self.apply_multi_cursor_insert_with_offset(text, 0);
    }
    fn apply_multi_cursor_insert_with_offset(&mut self, text: &str, offset: usize) {
        let positions = self.cursor_positions();
        let mut result = self.buffer_mut().insert_at_positions(&positions, text);
        if offset > 0 {
            for position in &mut result {
                position.column = position.column.saturating_sub(offset);
            }
        }
        self.set_multi_cursor_positions(result);
    }
    fn set_multi_cursor_positions(&mut self, mut positions: Vec<Position>) {
        if positions.is_empty() {
            return;
        }
        let primary = positions.remove(0);
        self.buffer_mut().set_cursor(primary.line, primary.column);
        self.secondary_cursors = positions;
    }
    fn add_secondary_cursor(&mut self, line_delta: isize) {
        let primary = self.cursor();
        let line = if line_delta.is_negative() {
            self.cursor_positions()
                .into_iter()
                .map(|position| position.line)
                .min()
                .unwrap_or(primary.line)
                .checked_sub(line_delta.unsigned_abs())
        } else {
            self.cursor_positions()
                .into_iter()
                .map(|position| position.line)
                .max()
                .unwrap_or(primary.line)
                .checked_add(line_delta as usize)
        };
        let Some(line) = line.filter(|line| *line < self.buffer().lines().len()) else {
            self.status_message = "No more lines in that direction".to_owned();
            return;
        };
        let column = primary
            .column
            .min(self.buffer().lines()[line].chars().count());
        let candidate = Position { line, column };
        if candidate == primary || self.secondary_cursors.contains(&candidate) {
            self.status_message = "Cursor already exists at that position".to_owned();
            return;
        }
        self.secondary_cursors.push(candidate);
        self.secondary_cursors
            .sort_by_key(|position| (position.line, position.column));
        self.status_message = format!("{} cursors", self.secondary_cursors.len() + 1);
    }
    fn select_next_occurrence(&mut self) {
        let Some(anchor) = self.selection_anchor else {
            self.status_message = "Select text before selecting the next occurrence".to_owned();
            return;
        };
        let cursor = self.cursor();
        let (start, end) = if (anchor.line, anchor.column) <= (cursor.line, cursor.column) {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        if start.line != end.line || start == end {
            self.status_message = "Select a non-empty single-line occurrence".to_owned();
            return;
        }
        let query = self.buffer().text_between(start, end);
        let line = &self.file_lines()[start.line];
        let characters: Vec<_> = line.chars().collect();
        let search_start = self
            .secondary_selections
            .last()
            .map_or(end.column, |(_, selected_end)| selected_end.column);
        let query_len = query.chars().count();
        let mut candidates = (search_start..=characters.len().saturating_sub(query_len))
            .chain(0..search_start.min(characters.len().saturating_sub(query_len) + 1));
        let Some(column) = candidates.find(|column| {
            let candidate = characters[*column..*column + query_len]
                .iter()
                .collect::<String>();
            candidate == query
                && !self.secondary_selections.iter().any(|(selected_start, _)| {
                    selected_start.line == start.line && selected_start.column == *column
                })
                && *column != start.column
        }) else {
            self.status_message = "No next occurrence".to_owned();
            return;
        };
        self.secondary_selections.push((
            Position {
                line: start.line,
                column,
            },
            Position {
                line: start.line,
                column: column + query_len,
            },
        ));
        self.status_message = format!("{} selections", self.secondary_selections.len() + 1);
    }
    fn split_selection_into_lines(&mut self) {
        let Some(anchor) = self.selection_anchor else {
            self.status_message = "Select multiple lines first".to_owned();
            return;
        };
        let cursor = self.cursor();
        let (start, end) = if (anchor.line, anchor.column) <= (cursor.line, cursor.column) {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        if start.line == end.line {
            self.status_message = "Select multiple lines first".to_owned();
            return;
        }
        let mut ranges = Vec::new();
        for line in start.line..=end.line {
            let length = self.file_lines()[line].chars().count();
            let range_start = if line == start.line {
                start.column.min(length)
            } else {
                0
            };
            let range_end = if line == end.line {
                end.column.min(length)
            } else {
                length
            };
            if range_start < range_end {
                ranges.push((
                    Position {
                        line,
                        column: range_start,
                    },
                    Position {
                        line,
                        column: range_end,
                    },
                ));
            }
        }
        let Some((primary_start, primary_end)) = ranges.first().copied() else {
            self.status_message = "Selection contains no line text".to_owned();
            return;
        };
        self.selection_anchor = Some(primary_start);
        self.buffer_mut()
            .set_cursor(primary_end.line, primary_end.column);
        self.secondary_selections = ranges.into_iter().skip(1).collect();
        self.secondary_cursors.clear();
        self.status_message = format!("{} line selections", self.secondary_selections.len() + 1);
    }
    fn replace_active_selections(&mut self, replacement: &str) {
        let Some(anchor) = self.selection_anchor else {
            return;
        };
        let mut ranges = vec![(anchor, self.cursor())];
        ranges.extend(self.secondary_selections.iter().copied());
        self.buffer_mut().replace_ranges(&ranges, replacement);
        self.selection_anchor = None;
        self.secondary_selections.clear();
        self.secondary_cursors.clear();
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
    }

    fn paste_at_selections(&mut self, clipboard: &str) {
        let ranges = self.active_selection_ranges();
        let pieces: Vec<_> = clipboard.split('\n').map(str::to_owned).collect();
        let replacements: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(index, (start, end))| {
                (
                    *start,
                    *end,
                    if pieces.len() == ranges.len() {
                        pieces[index].clone()
                    } else {
                        clipboard.to_owned()
                    },
                )
            })
            .collect();
        self.buffer_mut().apply_text_edits(&replacements);
        self.selection_anchor = None;
        self.secondary_selections.clear();
        self.secondary_cursors.clear();
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
    }

    fn paste_at_cursors(&mut self, clipboard: &str) {
        let positions = self.cursor_positions();
        let pieces: Vec<_> = clipboard.split('\n').map(str::to_owned).collect();
        let edits: Vec<_> = positions
            .iter()
            .enumerate()
            .map(|(index, position)| {
                (
                    *position,
                    *position,
                    if pieces.len() == positions.len() {
                        pieces[index].clone()
                    } else {
                        clipboard.to_owned()
                    },
                )
            })
            .collect();
        self.buffer_mut().apply_text_edits(&edits);
        self.secondary_cursors.clear();
        self.sync_lsp_for_buffer();
        self.refresh_search_results();
    }

    fn active_selection_ranges(&self) -> Vec<(Position, Position)> {
        let Some(anchor) = self.selection_anchor else {
            return self.secondary_selections.clone();
        };
        std::iter::once((anchor, self.cursor()))
            .chain(self.secondary_selections.iter().copied())
            .collect()
    }
    fn selected_line_range(&self, include_next: bool) -> (usize, usize) {
        let current = self.cursor().line;
        if let Some(anchor) = self.selection_anchor {
            if anchor.line != current {
                let start = anchor.line.min(current);
                return (start, anchor.line.max(current) + 1);
            }
        }
        let end = if include_next {
            (current + 2).min(self.file_lines().len())
        } else {
            current + 1
        };
        (current, end)
    }
    fn move_selected_lines(&mut self, upward: bool) {
        let (start, end) = self.selected_line_range(false);
        if !self.buffer_mut().move_lines(start, end, upward) {
            self.status_message = if upward {
                "Cannot move lines further up".to_owned()
            } else {
                "Cannot move lines further down".to_owned()
            };
            return;
        }
        if let Some(anchor) = &mut self.selection_anchor {
            if (start..end).contains(&anchor.line) {
                anchor.line = if upward {
                    anchor.line.saturating_sub(1)
                } else {
                    anchor.line + 1
                };
            }
        }
        self.secondary_cursors.clear();
        self.status_message = if upward {
            "Moved line(s) up".to_owned()
        } else {
            "Moved line(s) down".to_owned()
        };
    }
    fn join_selected_lines(&mut self) {
        let (start, end) = self.selected_line_range(true);
        if !self.buffer_mut().join_lines(start, end) {
            self.status_message = "No following line to join".to_owned();
            return;
        }
        self.selection_anchor = None;
        self.secondary_cursors.clear();
        self.status_message = "Joined line(s)".to_owned();
    }
    fn sort_selected_lines(&mut self, reverse: bool) {
        let (start, end) = self.selected_line_range(false);
        if !self.buffer_mut().sort_lines(start, end, reverse) {
            self.status_message = "Select at least two lines to sort".to_owned();
            return;
        }
        self.selection_anchor = None;
        self.secondary_cursors.clear();
        self.status_message = if reverse {
            "Sorted lines descending".to_owned()
        } else {
            "Sorted lines ascending".to_owned()
        };
    }
    fn transform_selected_lines(&mut self, uppercase: bool) {
        let (start, end) = self.selected_line_range(false);
        if !self
            .buffer_mut()
            .transform_lines_case(start, end, uppercase)
        {
            self.status_message = "No line selected".to_owned();
            return;
        }
        self.selection_anchor = None;
        self.secondary_cursors.clear();
        self.status_message = if uppercase {
            "Converted lines to uppercase".to_owned()
        } else {
            "Converted lines to lowercase".to_owned()
        };
    }
    fn toggle_fold(&mut self) {
        let line = self.cursor().line;
        if !self.is_line_foldable(line) {
            self.status_message = "Current line has no foldable block".to_owned();
            return;
        }
        if !self.folded_lines.insert(line) {
            self.folded_lines.remove(&line);
            self.status_message = format!("Expanded line {}", line + 1);
        } else {
            self.status_message = format!("Folded line {}", line + 1);
        }
    }
    fn move_cursor_vertically(&mut self, delta: isize) {
        let current = self.cursor();
        if self.word_wrap() {
            self.move_cursor_by_visual_row(delta);
            return;
        }
        let last_line = self.file_lines().len().saturating_sub(1);
        let candidate = (current.line as isize + delta).clamp(0, last_line as isize) as usize;
        let target = if delta > 0 {
            self.folded_lines
                .iter()
                .filter_map(|start| {
                    (*start <= candidate)
                        .then(|| self.fold_end(*start).map(|end| (*start, end)))
                        .flatten()
                })
                .filter(|(_, end)| candidate < *end)
                .map(|(_, end)| end)
                .max()
                .unwrap_or(candidate)
                .min(last_line)
        } else {
            self.folded_lines
                .iter()
                .filter_map(|start| {
                    self.fold_end(*start)
                        .filter(|end| candidate < *end && *start <= candidate)
                        .map(|_| *start)
                })
                .min()
                .unwrap_or(candidate)
        };
        let column = self
            .file_lines()
            .get(target)
            .map_or(0, |line| line.chars().count().min(current.column));
        self.buffer_mut().set_cursor(target, column);
    }
    fn move_cursor_by_page(&mut self, direction: isize) {
        for _ in 0..10 {
            self.move_cursor_vertically(direction.signum());
        }
    }
    fn insert_line_below(&mut self) {
        let line = self.cursor().line;
        let indentation: String = self.file_lines()[line]
            .chars()
            .take_while(|character| character.is_whitespace())
            .collect();
        self.buffer_mut().move_end();
        self.buffer_mut().insert(&format!("\n{indentation}"));
    }
    fn goto_matching_bracket(&mut self) {
        let Some(position) = self.matching_bracket_position() else {
            self.status_message = "No matching bracket found".to_owned();
            return;
        };
        self.buffer_mut().set_cursor(position.line, position.column);
        self.status_message = format!(
            "Matching bracket at {}:{}",
            position.line + 1,
            position.column + 1
        );
    }

    pub fn matching_bracket_position(&self) -> Option<Position> {
        let cursor = self.cursor();
        let lines = self.file_lines().to_vec();
        let character = lines.get(cursor.line)?.chars().nth(cursor.column)?;
        let (target, forward) = match character {
            '(' => (')', true),
            '[' => (']', true),
            '{' => ('}', true),
            ')' => ('(', false),
            ']' => ('[', false),
            '}' => ('{', false),
            '"' | '\'' => (character, true),
            _ => return None,
        };
        let found = if matches!(character, '"' | '\'') {
            (cursor.line..lines.len()).find_map(|line| {
                let start = if line == cursor.line {
                    cursor.column + 1
                } else {
                    0
                };
                lines[line]
                    .chars()
                    .enumerate()
                    .skip(start)
                    .find(|(_, value)| *value == target)
                    .map(|(column, _)| Position { line, column })
            })
        } else if forward {
            let mut depth = 0;
            (cursor.line..lines.len()).find_map(|line| {
                let start = if line == cursor.line {
                    cursor.column + 1
                } else {
                    0
                };
                for (column, value) in lines[line].chars().enumerate().skip(start) {
                    if value == character {
                        depth += 1;
                    } else if value == target {
                        if depth == 0 {
                            return Some(Position { line, column });
                        }
                        depth -= 1;
                    }
                }
                None
            })
        } else {
            let mut depth = 0;
            (0..=cursor.line).rev().find_map(|line| {
                let end = if line == cursor.line {
                    cursor.column
                } else {
                    lines[line].chars().count()
                };
                let characters: Vec<char> = lines[line].chars().collect();
                for (column, value) in characters.into_iter().enumerate().take(end).rev() {
                    if value == character {
                        depth += 1;
                    } else if value == target {
                        if depth == 0 {
                            return Some(Position { line, column });
                        }
                        depth -= 1;
                    }
                }
                None
            })
        };
        found
    }
    fn move_cursor_by_visual_row(&mut self, delta: isize) {
        let width = self.word_wrap_column().max(1);
        let current = self.cursor();
        let mut rows = Vec::new();
        for (line, text) in self.file_lines().iter().enumerate() {
            if self.is_line_hidden(line) {
                continue;
            }
            let row_count = wrapped_row_count(text, width);
            for row in 0..row_count {
                rows.push((line, row));
            }
        }
        if rows.is_empty() {
            return;
        }
        let current_row = rows
            .iter()
            .position(|(line, row)| {
                *line == current.line
                    && *row
                        == visual_row_for_column(
                            self.file_lines().get(*line).map_or("", String::as_str),
                            current.column,
                            width,
                        )
            })
            .unwrap_or(0);
        let target_row =
            (current_row as isize + delta).clamp(0, rows.len().saturating_sub(1) as isize) as usize;
        let (line, row) = rows[target_row];
        let desired_column =
            display_column(&self.file_lines()[current.line], current.column) % width;
        let column =
            column_for_visual_position(&self.file_lines()[line], row, desired_column, width);
        self.buffer_mut().set_cursor(line, column);
    }
    fn fold_end(&self, start: usize) -> Option<usize> {
        let lines = self.buffer().lines();
        if matches!(self.language(), "md" | "markdown") {
            let mut markdown_state = MarkdownDetailsScanState::default();
            let mut opening_tags = 0;
            for (index, line) in lines.iter().enumerate().take(start + 1) {
                let (opens, _) = markdown_details_tags(line, &mut markdown_state);
                if index == start {
                    opening_tags = opens;
                }
            }
            if opening_tags == 0 {
                return self.fold_end_by_indentation(lines, start);
            }
            let mut depth = 0usize;
            markdown_state = MarkdownDetailsScanState::default();
            for (index, line) in lines.iter().enumerate().skip(start) {
                let (opens, closes) = markdown_details_tags(line, &mut markdown_state);
                depth += opens;
                depth = depth.saturating_sub(closes);
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            return None;
        }
        self.fold_end_by_indentation(lines, start)
    }
    fn fold_end_by_indentation(&self, lines: &[String], start: usize) -> Option<usize> {
        let base_indent = indentation_width(lines.get(start)?);
        let mut found_child = false;
        for (index, line) in lines.iter().enumerate().skip(start + 1) {
            if line.trim().is_empty() {
                continue;
            }
            let indent = indentation_width(line);
            if indent > base_indent {
                found_child = true;
                continue;
            }
            return found_child.then_some(index);
        }
        found_child.then_some(lines.len())
    }
    fn comment_marker(&self) -> Option<&'static str> {
        match self.language() {
            "py" | "pyi" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" => Some("#"),
            "lua" => Some("--"),
            "c" | "cc" | "cpp" | "h" | "hpp" | "css" | "dart" | "go" | "java" | "js" | "jsx"
            | "kt" | "kts" | "php" | "rs" | "rb" | "scss" | "swift" | "ts" | "tsx" => Some("//"),
            _ => None,
        }
    }
    fn select_all(&mut self) {
        let last_line = self.buffer().lines().len().saturating_sub(1);
        let last_column = self.buffer().lines()[last_line].chars().count();
        self.selection_anchor = Some(Position { line: 0, column: 0 });
        self.buffer_mut().set_cursor(last_line, last_column);
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.status_message = "Selected entire document".to_owned();
    }
    fn open_search_from_editor(&mut self) {
        let inherited = self
            .active_selection_ranges()
            .into_iter()
            .map(|(start, end)| self.buffer().text_between(start, end))
            .find(|selected| !selected.is_empty() && !selected.contains('\n'));
        let inherited_selection = inherited.is_some();
        if let Some(selected) = inherited {
            self.search_query = selected;
        }
        self.replace_mode = false;
        self.focus = Focus::Search;
        self.sidebar_tab = SidebarTab::Search;
        if inherited_selection {
            // Seed Ctrl-F from the active buffer immediately. Workspace-wide
            // traversal happens on the next query edit/navigation, so opening
            // search remains responsive even when the file lives in a large
            // system directory such as macOS's shared temporary directory.
            let path = self.buffer().path().map(Path::to_path_buf);
            let text = self.buffer().lines().join("\n");
            self.search_results = path
                .as_deref()
                .map(|path| search_text_with_options(path, &text, &self.search_query, false))
                .unwrap_or_default();
            self.search_index = 0;
            self.request_search_refresh();
            self.status_message = format!("{} matches", self.search_results.len());
        } else {
            self.request_search_refresh();
            self.status_message = "Searching workspace…".to_owned();
        }
    }
    fn request_lsp(&mut self, method: &str) -> AppResult<()> {
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            self.status_message = "LSP unavailable for welcome buffer".to_owned();
            return Ok(());
        };
        let language = self.language().to_owned();
        if language.is_empty() {
            self.status_message = "LSP unavailable: unknown language".to_owned();
            return Ok(());
        }
        let uri = path_to_uri(&path);
        let cursor = self.buffer().cursor();
        let character = utf16_column(self.file_lines()[cursor.line].as_str(), cursor.column);
        let params = match method {
            "textDocument/completion" => {
                json!({"textDocument":{"uri":uri},"position":{"line":cursor.line,"character":character},"context":{"triggerKind":1}})
            }
            "textDocument/documentSymbol" => json!({"textDocument":{"uri":uri}}),
            "workspace/symbol" => json!({"query": self.search_query.clone()}),
            _ => {
                json!({"textDocument":{"uri":uri},"position":{"line":cursor.line,"character":character}})
            }
        };
        let Some(id) = self.lsp.request_async(&language, method, params)? else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        self.lsp_pending.insert(
            (language, id),
            PendingLspRequest {
                generation: self.lsp_generation,
                kind: PendingLspKind::Query(method.to_owned()),
                path,
                deadline: Instant::now() + Duration::from_secs(5),
            },
        );
        self.status_message = format!("LSP {method} running…");
        Ok(())
    }

    fn handle_lsp_result(&mut self, method: &str, path: &Path, result: JsonValue) -> AppResult<()> {
        match method {
            "textDocument/completion" => {
                let items = completion_items(&result);
                self.status_message = format!("LSP completion: {} item(s)", items.len());
                self.lsp_popup =
                    (!items.is_empty()).then_some(LspPopup::Completion { items, selected: 0 });
                if self.lsp_popup.is_some() {
                    self.begin_overlay();
                }
            }
            "textDocument/hover" => {
                self.status_message = summarize_lsp_result(method, &result);
                self.lsp_popup = (!result.is_null()).then(|| LspPopup::Info(hover_text(&result)));
                if self.lsp_popup.is_some() {
                    self.begin_overlay();
                }
            }
            "textDocument/signatureHelp" => {
                let text = signature_help_text(&result);
                self.status_message = if text.is_empty() {
                    "LSP signature help: no signature".to_owned()
                } else {
                    "LSP signature help".to_owned()
                };
                self.lsp_popup = (!text.is_empty()).then_some(LspPopup::Info(text));
                if self.lsp_popup.is_some() {
                    self.begin_overlay();
                }
            }
            "textDocument/documentSymbol" | "workspace/symbol" => {
                let items = document_symbols(&result, path);
                let label = if method == "workspace/symbol" {
                    "workspace symbols"
                } else {
                    "symbols"
                };
                self.status_message = format!("LSP {label}: {} item(s)", items.len());
                self.lsp_popup =
                    (!items.is_empty()).then_some(LspPopup::Symbols { items, selected: 0 });
                if self.lsp_popup.is_some() {
                    self.begin_overlay();
                }
            }
            "textDocument/definition"
            | "textDocument/implementation"
            | "textDocument/typeDefinition"
            | "textDocument/references" => {
                let locations = definition_locations(&result);
                if locations.len() > 1 {
                    let title = match method {
                        "textDocument/implementation" => "Implementations",
                        "textDocument/typeDefinition" => "Type definitions",
                        "textDocument/references" => "References",
                        _ => "Definitions",
                    };
                    self.status_message = format!(
                        "{} location(s) · select with Up/Down · Enter jump",
                        locations.len()
                    );
                    self.lsp_popup = Some(LspPopup::Locations {
                        title: title.to_owned(),
                        locations,
                        selected: 0,
                    });
                    self.begin_overlay();
                } else if let Some((path, line, column)) = locations.into_iter().next() {
                    match Buffer::open(&path) {
                        Ok(mut buffer) => {
                            buffer.set_cursor(line, column);
                            self.open_buffer(buffer);
                            self.sync_lsp_for_buffer();
                            self.close_lsp_popup();
                            let label = match method {
                                "textDocument/implementation" => "Implementation",
                                "textDocument/typeDefinition" => "Type definition",
                                "textDocument/references" => "Reference",
                                _ => "Definition",
                            };
                            self.status_message =
                                format!("{label}: {}:{}:{}", path.display(), line + 1, column + 1);
                        }
                        Err(error) => {
                            self.status_message = format!("Definition open failed: {error}");
                        }
                    }
                } else {
                    self.status_message = format!("LSP {method}: no location");
                }
            }
            _ => self.status_message = summarize_lsp_result(method, &result),
        }
        Ok(())
    }

    fn request_lsp_rename(&mut self, new_name: &str) -> AppResult<()> {
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            self.status_message = "LSP rename unavailable for welcome buffer".to_owned();
            return Ok(());
        };
        let language = self.language().to_owned();
        if language.is_empty() {
            self.status_message = "LSP rename unavailable: unknown language".to_owned();
            return Ok(());
        }
        let uri = path_to_uri(&path);
        let cursor = self.buffer().cursor();
        let character = utf16_column(self.file_lines()[cursor.line].as_str(), cursor.column);
        let Some(id) = self.lsp.request_async(
            &language,
            "textDocument/rename",
            json!({
                "textDocument":{"uri":uri.clone()},
                "position":{"line":cursor.line,"character":character},
                "newName":new_name
            }),
        )?
        else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        self.lsp_pending.insert(
            (language, id),
            PendingLspRequest {
                generation: self.lsp_generation,
                kind: PendingLspKind::Rename { uri },
                path,
                deadline: Instant::now() + Duration::from_secs(5),
            },
        );
        self.status_message = "LSP rename running…".to_owned();
        Ok(())
    }

    fn handle_lsp_rename_result(&mut self, uri: &str, result: JsonValue) -> AppResult<()> {
        let summary = workspace_edit_summary(&result);
        if summary.is_empty() {
            self.status_message = "LSP rename returned no edits".to_owned();
        } else {
            self.lsp_popup = Some(LspPopup::RenamePreview {
                uri: uri.to_owned(),
                edit: result,
                summary,
            });
            self.begin_overlay();
            self.status_message = "Review rename changes · Enter apply · Esc cancel".to_owned();
        }
        Ok(())
    }

    fn request_lsp_edits(&mut self, method: &str) -> AppResult<()> {
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            self.status_message = "LSP edits unavailable for welcome buffer".to_owned();
            return Ok(());
        };
        let language = self.language().to_owned();
        if language.is_empty() {
            self.status_message = "LSP edits unavailable: unknown language".to_owned();
            return Ok(());
        }
        let uri = path_to_uri(&path);
        let cursor = self.buffer().cursor();
        let selection = self.selection_anchor.map(|anchor| {
            let cursor = self.buffer().cursor();
            if (anchor.line, anchor.column) <= (cursor.line, cursor.column) {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            }
        });
        let range_formatting = method == "textDocument/formatting"
            && selection.is_some_and(|(start, end)| start != end);
        let request_method = if range_formatting {
            "textDocument/rangeFormatting"
        } else {
            method
        };
        let formatting_options = json!({
            "tabSize": self.settings.editor.tab_size.max(1),
            "insertSpaces": !self.settings.editor.use_tabs,
        });
        let params = if request_method == "textDocument/formatting" {
            json!({
                "textDocument":{"uri":uri.clone()},
                "options":formatting_options
            })
        } else if request_method == "textDocument/rangeFormatting" {
            let (start, end) = selection.expect("range formatting requires a selection");
            let start_character =
                utf16_column(self.file_lines()[start.line].as_str(), start.column);
            let end_character = utf16_column(self.file_lines()[end.line].as_str(), end.column);
            json!({
                "textDocument":{"uri":uri.clone()},
                "range":{
                    "start":{"line":start.line,"character":start_character},
                    "end":{"line":end.line,"character":end_character}
                },
                "options":formatting_options
            })
        } else {
            let character = utf16_column(self.file_lines()[cursor.line].as_str(), cursor.column);
            json!({
                "textDocument":{"uri":uri},
                "range":{"start":{"line":cursor.line,"character":0},"end":{"line":cursor.line,"character":character}},
                "context":{"diagnostics":[]}
            })
        };
        let Some(id) = self.lsp.request_async(&language, request_method, params)? else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        self.lsp_pending.insert(
            (language, id),
            PendingLspRequest {
                generation: self.lsp_generation,
                kind: PendingLspKind::Edits {
                    method: method.to_owned(),
                    request_method: request_method.to_owned(),
                    uri,
                },
                path,
                deadline: Instant::now() + Duration::from_secs(5),
            },
        );
        self.status_message = format!("LSP {method} running…");
        Ok(())
    }

    fn handle_lsp_edits_result(
        &mut self,
        method: &str,
        _request_method: &str,
        uri: &str,
        result: JsonValue,
    ) -> AppResult<()> {
        if method == "textDocument/codeAction" {
            let actions: Vec<_> = result
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|action| {
                    if action.get("edit").is_none() && action.get("command").is_none() {
                        return None;
                    }
                    let title = action
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Code action")
                        .to_owned();
                    Some((title, action.clone()))
                })
                .collect();
            if actions.is_empty() {
                self.status_message = "LSP codeAction returned no executable actions".to_owned();
                return Ok(());
            }
            if actions.len() > 1 {
                self.lsp_popup = Some(LspPopup::CodeActions {
                    items: actions.iter().map(|(title, _)| title.clone()).collect(),
                    edits: actions.into_iter().map(|(_, edit)| edit).collect(),
                    selected: 0,
                });
                self.begin_overlay();
                self.status_message = "Select a code action · Enter apply · Esc cancel".to_owned();
                return Ok(());
            }
            self.execute_lsp_code_action(uri, &actions[0].1)?;
            return Ok(());
        }
        let changed = apply_lsp_text_edits(self.buffer_mut(), uri, &result);
        if changed > 0 {
            self.sync_lsp_for_buffer();
            self.status_message = format!("LSP {} applied {} edit(s)", method, changed);
        } else {
            self.status_message = format!("LSP {} returned no edits", method);
        }
        if method == "textDocument/formatting" && self.format_on_save_pending {
            self.format_on_save_pending = false;
            let editor = self.settings.editor.clone();
            self.save_active_buffer_now(&editor)?;
        }
        Ok(())
    }

    fn execute_lsp_code_action(
        &mut self,
        current_uri: &str,
        action: &serde_json::Value,
    ) -> AppResult<()> {
        if let Some(edit) = action.get("edit") {
            let changed = self.apply_workspace_edit(current_uri, edit)?;
            self.status_message = format!("LSP codeAction applied {changed} edit(s)");
            return Ok(());
        }
        let Some(command) = action.get("command") else {
            self.status_message = "LSP codeAction has no edit or command".to_owned();
            return Ok(());
        };
        let language = self.language().to_owned();
        if language.is_empty() {
            self.status_message = "LSP codeAction unavailable: unknown language".to_owned();
            return Ok(());
        }
        let Some(params) = lsp_execute_command_params(command) else {
            self.status_message = "LSP codeAction command is malformed".to_owned();
            return Ok(());
        };
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            self.status_message = "LSP codeAction unavailable for welcome buffer".to_owned();
            return Ok(());
        };
        let Some(id) = self
            .lsp
            .request_async(&language, "workspace/executeCommand", params)?
        else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        self.lsp_pending.insert(
            (language, id),
            PendingLspRequest {
                generation: self.lsp_generation,
                kind: PendingLspKind::ExecuteCommand {
                    uri: current_uri.to_owned(),
                },
                path,
                deadline: Instant::now() + Duration::from_secs(5),
            },
        );
        self.status_message = "LSP codeAction command running…".to_owned();
        Ok(())
    }

    fn handle_lsp_execute_command_result(
        &mut self,
        current_uri: &str,
        result: JsonValue,
    ) -> AppResult<()> {
        let has_workspace_edit = result.as_object().is_some_and(|object| {
            object.contains_key("changes") || object.contains_key("documentChanges")
        });
        if has_workspace_edit {
            let changed = self.apply_workspace_edit(current_uri, &result)?;
            self.sync_lsp_for_buffer();
            self.status_message = format!("LSP codeAction command applied {changed} edit(s)");
        } else {
            self.status_message = "LSP codeAction command executed".to_owned();
        }
        Ok(())
    }

    fn apply_workspace_edit(
        &mut self,
        current_uri: &str,
        workspace_edit: &serde_json::Value,
    ) -> AppResult<usize> {
        enum ResourceOperation {
            Create {
                path: PathBuf,
                overwrite: bool,
            },
            Rename {
                old: PathBuf,
                new: PathBuf,
                overwrite: bool,
            },
            Delete {
                path: PathBuf,
                ignore_if_missing: bool,
            },
        }
        let mut plans: Vec<(String, Vec<LspTextEdit>)> = Vec::new();
        let mut resource_operations = Vec::new();
        if let Some(changes) = workspace_edit
            .get("changes")
            .and_then(serde_json::Value::as_object)
        {
            for (uri, edits) in changes {
                plans.push((uri.clone(), parse_lsp_text_edits(edits, uri)?));
            }
        }
        if let Some(document_changes) = workspace_edit
            .get("documentChanges")
            .and_then(serde_json::Value::as_array)
        {
            for document_change in document_changes {
                if let Some(kind) = document_change
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                {
                    let options = document_change.get("options");
                    let overwrite = options
                        .and_then(|options| options.get("overwrite"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    match kind {
                        "create" => {
                            let uri = document_change
                                .get("uri")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    crate::error::AppError::Lsp(
                                        "CreateFile is missing uri".to_owned(),
                                    )
                                })?;
                            let path = uri_to_path(uri).ok_or_else(|| {
                                crate::error::AppError::Lsp(format!(
                                    "workspace edit uses unsupported URI: {uri}"
                                ))
                            })?;
                            resource_operations.push(ResourceOperation::Create { path, overwrite });
                        }
                        "rename" => {
                            let old_uri = document_change
                                .get("oldUri")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    crate::error::AppError::Lsp(
                                        "RenameFile is missing oldUri".to_owned(),
                                    )
                                })?;
                            let new_uri = document_change
                                .get("newUri")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    crate::error::AppError::Lsp(
                                        "RenameFile is missing newUri".to_owned(),
                                    )
                                })?;
                            let old = uri_to_path(old_uri).ok_or_else(|| {
                                crate::error::AppError::Lsp(format!(
                                    "workspace edit uses unsupported URI: {old_uri}"
                                ))
                            })?;
                            let new = uri_to_path(new_uri).ok_or_else(|| {
                                crate::error::AppError::Lsp(format!(
                                    "workspace edit uses unsupported URI: {new_uri}"
                                ))
                            })?;
                            resource_operations.push(ResourceOperation::Rename {
                                old,
                                new,
                                overwrite,
                            });
                        }
                        "delete" => {
                            let uri = document_change
                                .get("uri")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    crate::error::AppError::Lsp(
                                        "DeleteFile is missing uri".to_owned(),
                                    )
                                })?;
                            let path = uri_to_path(uri).ok_or_else(|| {
                                crate::error::AppError::Lsp(format!(
                                    "workspace edit uses unsupported URI: {uri}"
                                ))
                            })?;
                            let ignore_if_missing = options
                                .and_then(|options| options.get("ignoreIfNotExists"))
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false);
                            resource_operations.push(ResourceOperation::Delete {
                                path,
                                ignore_if_missing,
                            });
                        }
                        other => {
                            return Err(crate::error::AppError::Lsp(format!(
                                "unsupported workspace resource operation: {other}"
                            )))
                        }
                    }
                    continue;
                }
                let Some(uri) = document_change
                    .get("textDocument")
                    .and_then(|document| document.get("uri"))
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let Some(edits) = document_change.get("edits") else {
                    continue;
                };
                plans.push((uri.to_owned(), parse_lsp_text_edits(edits, uri)?));
            }
        }

        let mut virtual_exists = BTreeMap::<PathBuf, bool>::new();
        for operation in &resource_operations {
            match operation {
                ResourceOperation::Create { path, overwrite } => {
                    let exists = *virtual_exists
                        .entry(path.clone())
                        .or_insert_with(|| path.exists());
                    if exists && !overwrite {
                        return Err(crate::error::AppError::Lsp(format!(
                            "CreateFile target already exists: {}",
                            path.display()
                        )));
                    }
                    if let Some(parent) = path.parent() {
                        if !parent.is_dir() {
                            return Err(crate::error::AppError::Lsp(format!(
                                "CreateFile parent does not exist: {}",
                                parent.display()
                            )));
                        }
                    }
                    virtual_exists.insert(path.clone(), true);
                }
                ResourceOperation::Rename {
                    old,
                    new,
                    overwrite,
                } => {
                    let old_exists = *virtual_exists
                        .entry(old.clone())
                        .or_insert_with(|| old.is_file());
                    let new_exists = *virtual_exists
                        .entry(new.clone())
                        .or_insert_with(|| new.exists());
                    if !old_exists {
                        return Err(crate::error::AppError::Lsp(format!(
                            "RenameFile source is unavailable: {}",
                            old.display()
                        )));
                    }
                    if new_exists && !overwrite {
                        return Err(crate::error::AppError::Lsp(format!(
                            "RenameFile target already exists: {}",
                            new.display()
                        )));
                    }
                    if old.exists() && fs::metadata(old)?.permissions().readonly() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "RenameFile source is read-only: {}",
                            old.display()
                        )));
                    }
                    virtual_exists.insert(old.clone(), false);
                    virtual_exists.insert(new.clone(), true);
                }
                ResourceOperation::Delete {
                    path,
                    ignore_if_missing,
                } => {
                    let exists = *virtual_exists
                        .entry(path.clone())
                        .or_insert_with(|| path.exists());
                    if exists && path.exists() && !path.is_file() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "DeleteFile target is not a regular file: {}",
                            path.display()
                        )));
                    }
                    if exists && path.exists() && fs::metadata(path)?.permissions().readonly() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "DeleteFile target is read-only: {}",
                            path.display()
                        )));
                    }
                    if !exists && !ignore_if_missing {
                        return Err(crate::error::AppError::Lsp(format!(
                            "DeleteFile target does not exist: {}",
                            path.display()
                        )));
                    }
                    virtual_exists.insert(path.clone(), false);
                }
            }
        }

        for (uri, edits) in &mut plans {
            let Some(path) = uri_to_path(uri) else {
                return Err(crate::error::AppError::Lsp(format!(
                    "workspace edit uses unsupported URI: {uri}"
                )));
            };
            if uri == current_uri {
                normalize_lsp_text_edits(self.buffer(), uri, edits)?;
                validate_lsp_text_edits(self.buffer(), uri, edits)?;
            } else {
                if let Some(buffer) = self
                    .buffers
                    .iter()
                    .find(|buffer| buffer.path() == Some(path.as_path()))
                {
                    if buffer.is_readonly() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "workspace edit target is read-only: {}",
                            path.display()
                        )));
                    }
                    if buffer.has_external_change() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "workspace edit target changed on disk: {}",
                            path.display()
                        )));
                    }
                    normalize_lsp_text_edits(buffer, uri, edits)?;
                    validate_lsp_text_edits(buffer, uri, edits)?;
                } else {
                    if !path.is_file() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "workspace edit target does not exist: {}",
                            path.display()
                        )));
                    }
                    if fs::metadata(&path)?.permissions().readonly() {
                        return Err(crate::error::AppError::Lsp(format!(
                            "workspace edit target is read-only: {}",
                            path.display()
                        )));
                    }
                    let buffer = Buffer::open(&path)?;
                    normalize_lsp_text_edits(&buffer, uri, edits)?;
                    validate_lsp_text_edits(&buffer, uri, edits)?;
                }
            }
        }

        let mut changed = 0;
        for (uri, edits) in plans {
            if uri == current_uri {
                changed += apply_parsed_lsp_text_edits(self.buffer_mut(), &edits);
            } else if let Some(path) = uri_to_path(&uri) {
                if let Some(index) = self
                    .buffers
                    .iter()
                    .position(|buffer| buffer.path() == Some(path.as_path()))
                {
                    changed += apply_parsed_lsp_text_edits(&mut self.buffers[index], &edits);
                } else {
                    let mut buffer = Buffer::open(&path)?;
                    let count = apply_parsed_lsp_text_edits(&mut buffer, &edits);
                    if count > 0 {
                        buffer.save()?;
                        changed += count;
                    }
                }
            }
        }
        let mut active_file_deleted = false;
        for operation in resource_operations {
            match operation {
                ResourceOperation::Create { path, overwrite } => {
                    if overwrite && path.exists() {
                        fs::write(&path, [])?;
                    } else {
                        fs::File::create(&path)?;
                    }
                    changed += 1;
                }
                ResourceOperation::Rename {
                    old,
                    new,
                    overwrite,
                } => {
                    let active_old_path = self
                        .buffer()
                        .path()
                        .filter(|open| open.strip_prefix(&old).is_ok())
                        .map(Path::to_path_buf);
                    let active_language =
                        active_old_path.as_ref().map(|_| self.language().to_owned());
                    if let (Some(language), Some(old_path)) =
                        (active_language.as_deref(), active_old_path.as_deref())
                    {
                        self.notify_lsp_closed(language, old_path);
                    }
                    if overwrite && new.exists() {
                        fs::remove_file(&new)?;
                    }
                    fs::rename(&old, &new)?;
                    self.relocate_open_buffers(&old, &new);
                    if active_old_path.is_some() {
                        self.sync_lsp_for_buffer();
                    }
                    changed += 1;
                }
                ResourceOperation::Delete { path, .. } => {
                    let active_old_path = self
                        .buffer()
                        .path()
                        .filter(|open| open.strip_prefix(&path).is_ok())
                        .map(Path::to_path_buf);
                    if let Some(old_path) = active_old_path.as_deref() {
                        let language = self.language().to_owned();
                        self.notify_lsp_closed(&language, old_path);
                        active_file_deleted = true;
                    }
                    if path.exists() {
                        fs::remove_file(&path)?;
                        for buffer in &mut self.buffers {
                            if buffer
                                .path()
                                .is_some_and(|open| open.strip_prefix(&path).is_ok())
                            {
                                let _ = buffer.poll_external_change();
                            }
                        }
                        changed += 1;
                    }
                }
            }
        }
        if active_file_deleted {
            self.status_message =
                "LSP deleted the active file; save to recreate or close the tab".to_owned();
        } else if changed > 0 {
            self.sync_lsp_for_buffer();
        }
        Ok(changed)
    }

    fn close_lsp_popup(&mut self) {
        self.lsp_popup = None;
        self.restore_overlay_focus();
    }

    fn open_conflict_preview(&mut self) {
        let blocks = conflict_blocks(self.file_lines());
        let line = self.cursor().line;
        let Some(block) = blocks
            .iter()
            .find(|block| (block.start_line..=block.end_line).contains(&line))
            .or_else(|| blocks.first())
            .cloned()
        else {
            self.status_message = "No inline conflict block found".to_owned();
            return;
        };
        let lines = self.file_lines().to_vec();
        let mut preview = vec!["Ours".to_owned()];
        preview.extend(
            lines[block.ours_start..block.ours_end]
                .iter()
                .map(|line| format!("  {line}")),
        );
        if let (Some(base_start), Some(base_end)) = (block.base_start, block.base_end) {
            preview.push("Base".to_owned());
            preview.extend(
                lines[base_start..base_end]
                    .iter()
                    .map(|line| format!("  {line}")),
            );
        }
        preview.push("Theirs".to_owned());
        preview.extend(
            lines[block.theirs_start..block.theirs_end]
                .iter()
                .map(|line| format!("  {line}")),
        );
        preview.push("".to_owned());
        preview.push("o Ours · t Theirs · b Base · k Keep Both · Esc close".to_owned());
        self.begin_overlay();
        self.lsp_popup = Some(LspPopup::ConflictPreview { lines: preview });
    }

    fn apply_completion_item(&mut self, item: &LspCompletionItem) {
        let lines = self.file_lines().to_vec();
        let primary_range = item.edit_range.clone().unwrap_or_else(|| {
            let cursor = self.buffer().cursor();
            LspCompletionRange {
                start_line: cursor.line,
                start_character: utf16_column(&lines[cursor.line], cursor.column),
                end_line: cursor.line,
                end_character: utf16_column(&lines[cursor.line], cursor.column),
            }
        });
        let mut edits = std::iter::once((primary_range, item.insert_text.clone()))
            .chain(
                item.additional_edits
                    .iter()
                    .map(|edit| (edit.range.clone(), edit.new_text.clone())),
            )
            .collect::<Vec<_>>();
        edits.sort_by(|(left, _), (right, _)| {
            (
                right.start_line,
                right.start_character,
                right.end_line,
                right.end_character,
            )
                .cmp(&(
                    left.start_line,
                    left.start_character,
                    left.end_line,
                    left.end_character,
                ))
        });
        for (range, text) in edits {
            let start_line = range.start_line.min(lines.len().saturating_sub(1));
            let end_line = range.end_line.min(lines.len().saturating_sub(1));
            let start_column = editor_column_from_utf16(&lines[start_line], range.start_character)
                .unwrap_or_else(|| lines[start_line].chars().count());
            let end_column = editor_column_from_utf16(&lines[end_line], range.end_character)
                .unwrap_or_else(|| lines[end_line].chars().count());
            self.buffer_mut().replace_ranges(
                &[(
                    Position {
                        line: start_line,
                        column: start_column,
                    },
                    Position {
                        line: end_line,
                        column: end_column,
                    },
                )],
                &text,
            );
        }
    }

    fn queue_completion_item(
        &mut self,
        item: LspCompletionItem,
        commit_character: Option<char>,
    ) -> AppResult<()> {
        let Some(params) = item.resolve_params.clone() else {
            self.apply_completion_item(&item);
            if let Some(character) = commit_character {
                self.buffer_mut().insert(&character.to_string());
            }
            self.sync_lsp_for_buffer();
            return Ok(());
        };
        let language = self.language().to_owned();
        let Some(path) = self.buffer().path().map(Path::to_path_buf) else {
            self.apply_completion_item(&item);
            return Ok(());
        };
        if language.is_empty() {
            self.apply_completion_item(&item);
            return Ok(());
        }
        let Some(id) = self
            .lsp
            .request_async(&language, "completionItem/resolve", params)?
        else {
            self.apply_completion_item(&item);
            if let Some(character) = commit_character {
                self.buffer_mut().insert(&character.to_string());
            }
            self.sync_lsp_for_buffer();
            return Ok(());
        };
        self.lsp_pending.insert(
            (language, id),
            PendingLspRequest {
                generation: self.lsp_generation,
                kind: PendingLspKind::CompletionResolve {
                    item,
                    commit_character,
                },
                path,
                deadline: Instant::now() + Duration::from_secs(5),
            },
        );
        self.status_message = "LSP completion resolve running…".to_owned();
        Ok(())
    }

    fn handle_completion_resolve_result(
        &mut self,
        item: LspCompletionItem,
        commit_character: Option<char>,
        result: JsonValue,
    ) -> AppResult<()> {
        let params = item.resolve_params.as_ref().ok_or_else(|| {
            crate::error::AppError::Lsp("completion resolve parameters disappeared".to_owned())
        })?;
        let merged = merge_completion_item(params, &result);
        let mut resolved = completion_items(&json!({"items": [merged]}));
        let item = resolved.pop().unwrap_or(item);
        self.apply_completion_item(&item);
        if let Some(character) = commit_character {
            self.buffer_mut().insert(&character.to_string());
        }
        self.sync_lsp_for_buffer();
        self.status_message = "LSP completion applied".to_owned();
        Ok(())
    }

    fn handle_lsp_popup_key(&mut self, key: KeyEvent) -> AppResult<bool> {
        let Some(popup) = &mut self.lsp_popup else {
            return Ok(false);
        };
        match popup {
            LspPopup::Completion { items, selected } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *selected = selected.saturating_sub(1);
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1));
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Char(character))
                    if items
                        .get(*selected)
                        .is_some_and(|item| item.commit_characters.contains(&character)) =>
                {
                    let item = items.get(*selected).cloned();
                    self.close_lsp_popup();
                    if let Some(item) = item {
                        self.queue_completion_item(item, Some(character))?;
                    }
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let item = items.get(*selected).cloned();
                    self.close_lsp_popup();
                    if let Some(item) = item {
                        self.queue_completion_item(item, None)?;
                    }
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.close_lsp_popup();
                    Ok(true)
                }
                _ => Ok(false),
            },
            LspPopup::CodeActions {
                items,
                edits,
                selected,
            } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *selected = selected.saturating_sub(1);
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1));
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let action = edits.get(*selected).cloned();
                    self.close_lsp_popup();
                    if let Some(action) = action {
                        let uri = self.buffer().path().map(path_to_uri);
                        if let Some(uri) = uri {
                            self.execute_lsp_code_action(&uri, &action)?;
                        }
                    }
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.close_lsp_popup();
                    Ok(true)
                }
                _ => Ok(false),
            },
            LspPopup::Locations {
                locations,
                selected,
                ..
            } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *selected = selected.saturating_sub(1);
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    *selected = (*selected + 1).min(locations.len().saturating_sub(1));
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let location = locations.get(*selected).cloned();
                    self.close_lsp_popup();
                    if let Some((path, line, column)) = location {
                        match Buffer::open(&path) {
                            Ok(mut buffer) => {
                                buffer.set_cursor(line, column);
                                self.open_buffer(buffer);
                                self.sync_lsp_for_buffer();
                                self.status_message = format!(
                                    "LSP location: {}:{}:{}",
                                    path.display(),
                                    line + 1,
                                    column + 1
                                );
                            }
                            Err(error) => {
                                self.status_message = format!("LSP location open failed: {error}");
                            }
                        }
                    }
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.close_lsp_popup();
                    Ok(true)
                }
                _ => Ok(false),
            },
            LspPopup::Symbols { items, selected } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *selected = selected.saturating_sub(1);
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1));
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let location = items
                        .get(*selected)
                        .map(|(_, path, line, column)| (path.clone(), *line, *column));
                    self.close_lsp_popup();
                    if let Some((path, line, column)) = location {
                        match Buffer::open(&path) {
                            Ok(mut buffer) => {
                                buffer.set_cursor(line, column);
                                self.open_buffer(buffer);
                                self.sync_lsp_for_buffer();
                                self.status_message = format!(
                                    "LSP symbol: {}:{}:{}",
                                    path.display(),
                                    line + 1,
                                    column + 1
                                );
                            }
                            Err(error) => {
                                self.status_message = format!("LSP symbol open failed: {error}");
                            }
                        }
                    }
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.close_lsp_popup();
                    Ok(true)
                }
                _ => Ok(false),
            },
            LspPopup::RenamePreview { uri, edit, .. } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let uri = uri.clone();
                    let edit = edit.clone();
                    let changed = match self.apply_workspace_edit(&uri, &edit) {
                        Ok(changed) => changed,
                        Err(error) => {
                            self.status_message = format!("LSP rename not applied: {error}");
                            return Ok(true);
                        }
                    };
                    self.close_lsp_popup();
                    if changed > 0 {
                        self.sync_lsp_for_buffer();
                        self.status_message = format!("Renamed {changed} occurrence(s)");
                    } else {
                        self.status_message = "LSP rename returned no edits".to_owned();
                    }
                    Ok(true)
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.close_lsp_popup();
                    self.status_message = "LSP rename cancelled".to_owned();
                    Ok(true)
                }
                _ => Ok(false),
            },
            LspPopup::ConflictPreview { .. } => match key.code {
                KeyCode::Char('o') => {
                    self.close_lsp_popup();
                    self.resolve_current_conflict(true);
                    Ok(true)
                }
                KeyCode::Char('t') => {
                    self.close_lsp_popup();
                    self.resolve_current_conflict(false);
                    Ok(true)
                }
                KeyCode::Char('b') => {
                    self.close_lsp_popup();
                    self.resolve_current_conflict_base();
                    Ok(true)
                }
                KeyCode::Char('k') => {
                    self.close_lsp_popup();
                    self.resolve_current_conflict_both();
                    Ok(true)
                }
                KeyCode::Esc => {
                    self.close_lsp_popup();
                    Ok(true)
                }
                _ => Ok(false),
            },
            LspPopup::Info(_) => {
                if matches!(key.code, KeyCode::Esc) {
                    self.close_lsp_popup();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }
    fn handle_navigation(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => self.focus_next(),
            (KeyModifiers::SHIFT, KeyCode::BackTab) => self.focus_previous(),
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                self.sidebar_visible = !self.sidebar_visible;
                self.status_message = if self.sidebar_visible {
                    "Sidebar shown"
                } else {
                    "Sidebar hidden"
                }
                .to_owned();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                if self.terminal_fullscreen {
                    self.toggle_terminal_fullscreen();
                    return;
                }
                self.bottom_visible = !self.bottom_visible;
                self.status_message = if self.bottom_visible {
                    "Bottom panel shown"
                } else {
                    "Bottom panel hidden"
                }
                .to_owned();
            }
            (KeyModifiers::ALT, KeyCode::Char('t')) => self.toggle_terminal_fullscreen(),
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.agent_visible = !self.agent_visible;
                self.status_message = if self.agent_visible {
                    "AI Agent panel shown"
                } else {
                    "AI Agent panel hidden"
                }
                .to_owned();
            }
            (modifiers, KeyCode::Char('t'))
                if modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.cycle_theme();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => self.open_plugin_palette(),
            (KeyModifiers::NONE, KeyCode::Char('1')) => self.select_sidebar(SidebarTab::Explorer),
            (KeyModifiers::NONE, KeyCode::Char('2')) => self.select_sidebar(SidebarTab::Search),
            (KeyModifiers::NONE, KeyCode::Char('3')) => self.select_sidebar(SidebarTab::Changes),
            (KeyModifiers::NONE, KeyCode::Char('4')) => self.select_sidebar(SidebarTab::Branches),
            (KeyModifiers::NONE, KeyCode::Char('5')) => {
                self.bottom_tab = BottomTab::Terminal;
                self.focus = Focus::Terminal;
            }
            (KeyModifiers::NONE, KeyCode::Char('6')) => {
                self.bottom_tab = BottomTab::Output;
                self.focus = Focus::Output;
            }
            (KeyModifiers::NONE, KeyCode::Esc) => self.focus = Focus::Editor,
            _ => {}
        }
    }
    fn toggle_terminal_fullscreen(&mut self) {
        if self.terminal_fullscreen {
            self.sidebar_visible = self.fullscreen_sidebar_visible;
            self.agent_visible = self.fullscreen_agent_visible;
            self.terminal_fullscreen = false;
            self.focus = Focus::Editor;
            self.status_message = "Terminal fullscreen closed".to_owned();
        } else {
            self.fullscreen_sidebar_visible = self.sidebar_visible;
            self.fullscreen_agent_visible = self.agent_visible;
            self.sidebar_visible = false;
            self.agent_visible = false;
            self.bottom_visible = true;
            self.terminal_fullscreen = true;
            self.bottom_tab = BottomTab::Terminal;
            self.focus = Focus::Terminal;
            self.status_message = "Terminal fullscreen · Alt-T restore".to_owned();
        }
    }
    fn focus_next(&mut self) {
        let visible = Focus::ALL.into_iter().filter(|focus| match focus {
            Focus::Explorer | Focus::Search | Focus::Changes | Focus::Branches => {
                self.sidebar_visible
            }
            Focus::Terminal | Focus::Output => self.bottom_visible,
            Focus::Agent => self.agent_visible,
            Focus::Editor => true,
        });
        let mut passed = false;
        for focus in visible {
            if passed {
                self.focus = focus;
                return;
            }
            passed = focus == self.focus;
        }
        self.focus = Focus::Editor;
    }
    fn focus_previous(&mut self) {
        let visible: Vec<_> = Focus::ALL
            .into_iter()
            .filter(|focus| match focus {
                Focus::Explorer | Focus::Search | Focus::Changes | Focus::Branches => {
                    self.sidebar_visible
                }
                Focus::Terminal | Focus::Output => self.bottom_visible,
                Focus::Agent => self.agent_visible,
                Focus::Editor => true,
            })
            .collect();
        let index = visible.iter().position(|focus| *focus == self.focus);
        self.focus = index
            .and_then(|i| i.checked_sub(1).and_then(|p| visible.get(p).copied()))
            .unwrap_or_else(|| visible.last().copied().unwrap_or(Focus::Editor));
    }
    fn select_sidebar(&mut self, tab: SidebarTab) {
        self.sidebar_tab = tab;
        if tab == SidebarTab::Problems {
            self.problem_index = self
                .problem_index
                .min(self.problems().len().saturating_sub(1));
        }
        self.focus = match tab {
            SidebarTab::Explorer => Focus::Explorer,
            SidebarTab::Search => Focus::Search,
            SidebarTab::Changes => Focus::Changes,
            SidebarTab::Branches => Focus::Branches,
            SidebarTab::Problems => Focus::Explorer,
        };
    }
}

fn collect_agent_search_results(
    path: &Path,
    query: &str,
    limit: usize,
    results: &mut Vec<JsonValue>,
) {
    if results.len() >= limit {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect_agent_search_results(&entry.path(), query, limit, results);
            if results.len() >= limit {
                break;
            }
        }
    } else if metadata.is_file() {
        let Ok(contents) = fs::read_to_string(path) else {
            return;
        };
        for (index, line) in contents.lines().enumerate() {
            if line.contains(query) {
                results.push(json!({
                    "path": path.display().to_string(),
                    "line": index + 1,
                    "text": line,
                }));
                if results.len() >= limit {
                    break;
                }
            }
        }
    }
}

fn agent_tool_error_response(id: JsonValue, error: impl Into<String>) -> JsonValue {
    json!({
        "type": "tool_result",
        "id": id,
        "ok": false,
        "error": error.into(),
    })
}

fn execute_agent_tool_call(
    tool: &str,
    args: &JsonValue,
    allowed_tools: &[String],
    workspace_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> AppResult<JsonValue> {
    if !allowed_tools.iter().any(|allowed| allowed == tool) {
        return Err(crate::error::AppError::Workspace(format!(
            "agent tool denied: {tool}"
        )));
    }
    let path = args
        .get("path")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| crate::error::AppError::Workspace("agent tool path is required".into()))?;
    let search_all_workspaces = tool == "search" && path == ".";
    let resolved_path = if tool == "write_file" {
        agent_workspace_write_path(path, workspace_roots)?
    } else {
        agent_workspace_path(path, workspace_roots)?
    };
    match tool {
        "read_file" => {
            let size = fs::metadata(&resolved_path)?.len();
            if size > AGENT_TOOL_MAX_FILE_BYTES {
                return Err(crate::error::AppError::Workspace(format!(
                    "agent read_file exceeds {} MiB limit",
                    AGENT_TOOL_MAX_FILE_BYTES / (1024 * 1024)
                )));
            }
            let contents = fs::read_to_string(&resolved_path)?;
            Ok(json!({"path": resolved_path.display().to_string(), "content": contents}))
        }
        "list_files" => {
            let mut entries = fs::read_dir(&resolved_path)?
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            entries.sort();
            Ok(json!({"path": resolved_path.display().to_string(), "entries": entries}))
        }
        "write_file" => {
            let canonical_target = resolved_path
                .canonicalize()
                .unwrap_or_else(|_| resolved_path.clone());
            if protected_paths
                .iter()
                .any(|protected| protected == &canonical_target)
            {
                return Err(crate::error::AppError::Workspace(
                    "agent write_file blocked: open buffer has unsaved changes".into(),
                ));
            }
            let content = args
                .get("content")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    crate::error::AppError::Workspace("agent write_file content is required".into())
                })?;
            if content.len() as u64 > AGENT_TOOL_MAX_FILE_BYTES {
                return Err(crate::error::AppError::Workspace(format!(
                    "agent write_file exceeds {} MiB limit",
                    AGENT_TOOL_MAX_FILE_BYTES / (1024 * 1024)
                )));
            }
            let temporary =
                resolved_path.with_extension(format!("mtide-agent-{}", std::process::id()));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            if let Err(error) = file.write_all(content.as_bytes()) {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
            drop(file);
            if let Err(error) = fs::rename(&temporary, &resolved_path) {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
            Ok(json!({"path": resolved_path.display().to_string(), "bytes": content.len()}))
        }
        "search" => {
            let query = args
                .get("query")
                .and_then(JsonValue::as_str)
                .filter(|query| !query.is_empty())
                .ok_or_else(|| {
                    crate::error::AppError::Workspace("agent search query is required".into())
                })?;
            let limit = args
                .get("limit")
                .and_then(JsonValue::as_u64)
                .unwrap_or(50)
                .clamp(1, 500) as usize;
            let mut results = Vec::new();
            if search_all_workspaces {
                for root in workspace_roots {
                    collect_agent_search_results(root, query, limit, &mut results);
                    if results.len() >= limit {
                        break;
                    }
                }
            } else {
                collect_agent_search_results(&resolved_path, query, limit, &mut results);
            }
            Ok(json!({"query": query, "results": results}))
        }
        _ => Err(crate::error::AppError::Workspace(format!(
            "unknown agent tool: {tool}"
        ))),
    }
}

fn agent_workspace_candidates(path: &str, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    if Path::new(path).is_absolute() {
        vec![PathBuf::from(path)]
    } else {
        workspace_roots.iter().map(|root| root.join(path)).collect()
    }
}

fn agent_path_in_workspace(path: &Path, workspace_roots: &[PathBuf]) -> bool {
    workspace_roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| path.starts_with(root))
}

fn agent_workspace_path(path: &str, workspace_roots: &[PathBuf]) -> AppResult<PathBuf> {
    let mut last_error = None;
    for candidate in agent_workspace_candidates(path, workspace_roots) {
        match candidate.canonicalize() {
            Ok(canonical) => {
                if agent_path_in_workspace(&canonical, workspace_roots) {
                    return Ok(canonical);
                }
                last_error = Some(crate::error::AppError::Workspace(
                    "agent tool path is outside workspace".into(),
                ));
            }
            Err(error) => last_error = Some(error.into()),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        crate::error::AppError::Workspace("agent tool path is outside workspace".into())
    }))
}

fn agent_workspace_write_path(path: &str, workspace_roots: &[PathBuf]) -> AppResult<PathBuf> {
    for candidate in agent_workspace_candidates(path, workspace_roots) {
        if candidate.exists() {
            let canonical = candidate.canonicalize()?;
            if canonical.is_file() && agent_path_in_workspace(&canonical, workspace_roots) {
                return Ok(canonical);
            }
            continue;
        }
        let Some(parent) = candidate.parent() else {
            continue;
        };
        let Ok(parent) = parent.canonicalize() else {
            continue;
        };
        if !agent_path_in_workspace(&parent, workspace_roots) {
            continue;
        }
        let Some(name) = candidate.file_name() else {
            continue;
        };
        return Ok(parent.join(name));
    }
    Err(crate::error::AppError::Workspace(
        "agent write_file path is outside workspace or unavailable".into(),
    ))
}

fn split_unified_diff_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.starts_with("-") && !line.starts_with("---") {
                format!("{line:<42} │")
            } else if line.starts_with('+') && !line.starts_with("+++") {
                format!("{: <42} │ {line}", "")
            } else {
                format!("{line:<42} │ {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn collapse_plugin_diff_hunks(text: &str, collapsed: &BTreeSet<usize>) -> String {
    let mut hunk = None;
    let mut marker_added = false;
    let mut output = Vec::new();
    for line in text.lines() {
        if line.starts_with("@@") {
            hunk = Some(hunk.map_or(0, |index: usize| index + 1));
            marker_added = false;
            output.push(line.to_owned());
            continue;
        }
        if hunk.is_some_and(|index| collapsed.contains(&index)) {
            if !marker_added {
                output.push("… [hunk collapsed]".to_owned());
                marker_added = true;
            }
        } else {
            output.push(line.to_owned());
        }
    }
    output.join("\n")
}

fn list_directory(
    workspace_root: &Path,
    current: &Path,
    show_hidden: bool,
    show_git_ignored: bool,
) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(current) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            (!hidden || show_hidden)
                && (show_git_ignored || !quick_open_is_git_ignored(workspace_root, path))
        })
        .map(|path| {
            let is_file = !path.is_dir();
            let name = path.file_name().map(|name| name.to_os_string());
            (is_file, name, path)
        })
        .collect();
    paths.sort_by_key(|(is_file, name, _)| (*is_file, name.clone()));
    paths.into_iter().map(|(_, _, path)| path).collect()
}

fn collect_quick_open_files(
    root: &Path,
    current: &Path,
    show_hidden: bool,
    show_git_ignored: bool,
    output: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if (!show_hidden && name.starts_with('.'))
            || (!show_hidden && matches!(name, "target" | "node_modules" | "vendor"))
        {
            continue;
        }
        if !show_git_ignored && quick_open_is_git_ignored(root, &path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            collect_quick_open_files(root, &path, show_hidden, show_git_ignored, output);
        } else if metadata.is_file() {
            output.push(path);
        }
    }
}

fn quick_open_is_git_ignored(root: &Path, path: &Path) -> bool {
    if !root.join(".git").exists() && !root.join(".gitignore").is_file() {
        return false;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    std::process::Command::new("git")
        .args([
            "-C",
            &root.to_string_lossy(),
            "check-ignore",
            "--quiet",
            "--no-index",
            "--",
        ])
        .arg(relative)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn quick_open_match_score(path: &str, query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    let mut score = 0;
    let mut cursor = 0;
    for character in query.chars() {
        let Some(relative) = path[cursor..].find(character) else {
            return usize::MAX;
        };
        score += relative;
        cursor += relative + character.len_utf8();
    }
    score
}

fn quick_open_path_label(path: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .map_or_else(
            || path.to_string_lossy().to_ascii_lowercase(),
            |relative| relative.to_string_lossy().to_ascii_lowercase(),
        )
}

fn workspace_git_changes(roots: &[PathBuf]) -> (Vec<GitChange>, Vec<PathBuf>) {
    let mut changes = Vec::new();
    let mut change_roots = Vec::new();
    for root in roots {
        let backend = GitBackend::new(root);
        for change in backend.status() {
            changes.push(change);
            change_roots.push(backend.root().to_path_buf());
        }
    }
    (changes, change_roots)
}

fn visible_explorer_entries(
    root: &Path,
    expanded: &BTreeSet<PathBuf>,
    show_hidden: bool,
    show_git_ignored: bool,
) -> Vec<PathBuf> {
    let mut output = Vec::new();
    fn visit(
        workspace_root: &Path,
        path: &Path,
        expanded: &BTreeSet<PathBuf>,
        show_hidden: bool,
        show_git_ignored: bool,
        output: &mut Vec<PathBuf>,
    ) {
        for child in list_directory(workspace_root, path, show_hidden, show_git_ignored) {
            output.push(child.clone());
            if child.is_dir() && expanded.contains(&child) {
                visit(
                    workspace_root,
                    &child,
                    expanded,
                    show_hidden,
                    show_git_ignored,
                    output,
                );
            }
        }
    }
    visit(
        root,
        root,
        expanded,
        show_hidden,
        show_git_ignored,
        &mut output,
    );
    output
}

fn indentation_width(line: &str) -> usize {
    line.chars()
        .take_while(|character| *character == '\t' || *character == ' ')
        .map(|character| if character == '\t' { 4 } else { 1 })
        .sum()
}

fn format_key_modifier(modifiers: KeyModifiers) -> String {
    let mut values = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        values.push("ctrl");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        values.push("alt");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        values.push("shift");
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        values.push("super");
    }
    values.join("+")
}

fn automation_key_events(input: &str) -> Result<Vec<KeyEvent>, String> {
    input
        .split_whitespace()
        .map(|token| {
            let mut modifiers = KeyModifiers::NONE;
            let mut key_name = String::new();
            for part in token.split('+') {
                let part = part.to_ascii_lowercase();
                match part.as_str() {
                    "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                    "alt" | "meta" => modifiers |= KeyModifiers::ALT,
                    "shift" => modifiers |= KeyModifiers::SHIFT,
                    "super" | "cmd" | "command" => modifiers |= KeyModifiers::SUPER,
                    value => key_name = value.to_owned(),
                }
            }
            let code = match key_name.as_str() {
                "enter" | "return" => KeyCode::Enter,
                "esc" | "escape" => KeyCode::Esc,
                "backspace" => KeyCode::Backspace,
                "delete" | "del" => KeyCode::Delete,
                "tab" => KeyCode::Tab,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" | "pgup" => KeyCode::PageUp,
                "pagedown" | "pgdown" | "pgdn" => KeyCode::PageDown,
                "space" => KeyCode::Char(' '),
                value if value.chars().count() == 1 => {
                    KeyCode::Char(value.chars().next().expect("single-character key"))
                }
                _ => return Err(format!("unknown automation key: {token}")),
            };
            Ok(KeyEvent::new(code, modifiers))
        })
        .collect()
}

fn automation_wait_for(input: &str) -> Result<(String, Duration), String> {
    let mut parts = input.split_whitespace().collect::<Vec<_>>();
    let mut timeout_ms = 5_000_u64;
    if let Some(last) = parts.last().copied() {
        if let Some(value) = last.strip_prefix("timeout=") {
            timeout_ms = value
                .parse::<u64>()
                .map_err(|_| "wait-for timeout must be an integer".to_owned())?;
            if timeout_ms > 30_000 {
                return Err("wait-for timeout must be <= 30000ms".to_owned());
            }
            parts.pop();
        }
    }
    let needle = parts.join(" ");
    if needle.is_empty() {
        return Err("wait-for requires screen text".to_owned());
    }
    Ok((needle, Duration::from_millis(timeout_ms)))
}

fn automation_mouse_coordinates(input: &str, count: usize) -> Result<[u16; 4], String> {
    if count != 2 && count != 4 {
        return Err("invalid automation mouse coordinate count".to_owned());
    }
    let values = input
        .split_whitespace()
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|_| format!("invalid mouse coordinate: {value}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() != count {
        return Err(format!(
            "expected {count} mouse coordinates, got {}",
            values.len()
        ));
    }
    Ok([
        values[0],
        values[1],
        values.get(2).copied().unwrap_or(values[0]),
        values.get(3).copied().unwrap_or(values[1]),
    ])
}

fn automation_mouse_events(action: &str, input: &str) -> Result<Vec<MouseEvent>, String> {
    let [column, row, _, _] = automation_mouse_coordinates(input, 2)?;
    let kind = match action {
        "hover" => {
            return Ok(vec![MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }])
        }
        "rclick" | "right_click" => MouseButton::Right,
        "click" => MouseButton::Left,
        _ => return Err(format!("unsupported automation mouse action: {action}")),
    };
    Ok(vec![
        MouseEvent {
            kind: MouseEventKind::Down(kind),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        MouseEvent {
            kind: MouseEventKind::Up(kind),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    ])
}

fn automation_scroll_coordinates(input: &str) -> Result<(u16, u16, i32), String> {
    let values = input.split_whitespace().collect::<Vec<_>>();
    if values.len() != 3 {
        return Err("expected x y signed-count for scroll".to_owned());
    }
    let x = values[0]
        .parse::<u16>()
        .map_err(|_| format!("invalid scroll x coordinate: {}", values[0]))?;
    let y = values[1]
        .parse::<u16>()
        .map_err(|_| format!("invalid scroll y coordinate: {}", values[1]))?;
    let amount = values[2]
        .parse::<i32>()
        .map_err(|_| format!("invalid scroll count: {}", values[2]))?;
    if amount.unsigned_abs() > 1000 {
        return Err("scroll count magnitude must be at most 1000".to_owned());
    }
    Ok((x, y, amount))
}

fn key_chord(key: KeyEvent) -> Option<String> {
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    let prefix = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "ctrl-"
    } else if key.modifiers.contains(KeyModifiers::ALT) {
        "alt-"
    } else if key.modifiers.contains(KeyModifiers::SHIFT) {
        "shift-"
    } else {
        ""
    };
    Some(format!("{prefix}{character}"))
}

fn editor_plugin_key_name(key: KeyEvent) -> String {
    let name = match key.code {
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PgUp".to_owned(),
        KeyCode::PageDown => "PgDn".to_owned(),
        _ => format!("{:?}", key.code),
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return format!("Ctrl-{}", name.to_ascii_uppercase());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        return format!("Alt-{name}");
    }
    name
}

fn terminal_control_byte(character: char) -> Option<u8> {
    match character {
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        character if character.is_ascii_alphabetic() => {
            Some(character.to_ascii_uppercase() as u8 & 0x1f)
        }
        _ => None,
    }
}

fn current_word_at_cursor(line: &str, column: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let cursor = column.min(chars.len());
    let index = if cursor < chars.len() && is_word_char(chars[cursor]) {
        cursor
    } else if cursor > 0 && is_word_char(chars[cursor - 1]) {
        cursor - 1
    } else {
        return String::new();
    };
    let mut start = index;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }
    chars[start..end].iter().collect()
}

fn is_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn safe_markdown_image_dir(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    if value.trim().is_empty() || path.is_absolute() {
        return None;
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => safe.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!safe.as_os_str().is_empty()).then_some(safe)
}

fn read_clipboard_png() -> Option<Vec<u8>> {
    // Keep the provider-specific probing explicit: no shell is involved, and
    // text clipboards simply fall through to the existing text paste path.
    let providers: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--no-newline", "--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (program, args) in providers {
        let Ok(output) = Command::new(program).args(args).output() else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            return Some(output.stdout);
        }
    }
    None
}

fn terminal_modified_key(key: KeyEvent) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let has_extended_modifier = modifiers.intersects(
        KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
    );
    if !has_extended_modifier {
        return None;
    }
    if modifiers == KeyModifiers::ALT {
        if let KeyCode::Char(character) = key.code {
            let mut bytes = vec![0x1b];
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            return Some(bytes);
        }
    }
    let modifier = 1
        + u8::from(modifiers.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
        + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL))
        + 8 * u8::from(modifiers.contains(KeyModifiers::SUPER));
    let (prefix, suffix) = match key.code {
        KeyCode::Up => ("A", ""),
        KeyCode::Down => ("B", ""),
        KeyCode::Right => ("C", ""),
        KeyCode::Left => ("D", ""),
        KeyCode::Home => ("H", ""),
        KeyCode::End => ("F", ""),
        KeyCode::PageUp => return Some(format!("\x1b[5;{modifier}~").into_bytes()),
        KeyCode::PageDown => return Some(format!("\x1b[6;{modifier}~").into_bytes()),
        KeyCode::Insert => return Some(format!("\x1b[2;{modifier}~").into_bytes()),
        KeyCode::Delete => return Some(format!("\x1b[3;{modifier}~").into_bytes()),
        KeyCode::F(number) if (1..=4).contains(&number) => {
            return Some(format!("\x1b[1;{modifier}{}", (b'P' + number - 1) as char).into_bytes())
        }
        KeyCode::F(number) if (5..=12).contains(&number) => {
            let code = match number {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => unreachable!(),
            };
            return Some(format!("\x1b[{code};{modifier}~").into_bytes());
        }
        KeyCode::Tab | KeyCode::BackTab if modifiers.contains(KeyModifiers::SHIFT) => {
            return Some(b"\x1b[Z".to_vec())
        }
        _ => return None,
    };
    Some(format!("\x1b[1;{modifier}{prefix}{suffix}").into_bytes())
}

fn vt_function_key(number: u8) -> &'static [u8] {
    match number {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => b"",
    }
}

fn terminal_command(settings: &Settings) -> Vec<String> {
    let shell = settings
        .terminal
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "sh".to_owned());
    let mut command = vec![shell];
    command.extend(settings.terminal.args.iter().cloned());
    if !command.iter().skip(1).any(|argument| argument == "-i") {
        command.push("-i".to_owned());
    }
    command
}

fn executable_available(executable: &str) -> bool {
    let path = Path::new(executable);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(executable).is_file())
    })
}

fn configured_agent_command(
    settings: &Settings,
    provider_name: &str,
) -> (Vec<String>, Option<PathBuf>, bool) {
    let (mut command, cwd, persistent) = settings
        .agent
        .providers
        .get(provider_name)
        .map(|provider| {
            (
                provider.command.clone(),
                provider.cwd.clone(),
                provider.persist,
            )
        })
        .unwrap_or_else(|| {
            (
                settings.agent.command.clone(),
                settings.agent.cwd.clone(),
                settings.agent.persist,
            )
        });
    if !persistent || !executable_available("tmux") {
        return (command, cwd.map(PathBuf::from), false);
    }
    if command.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned());
        command = vec![shell, "-i".to_owned()];
    }
    let session_name = agent_tmux_session_name(provider_name, cwd.as_deref());
    let mut wrapped = vec![
        "tmux".to_owned(),
        "new-session".to_owned(),
        "-A".to_owned(),
        "-s".to_owned(),
        session_name,
    ];
    if let Some(cwd) = cwd.as_deref() {
        wrapped.push("-c".to_owned());
        wrapped.push(cwd.to_owned());
    }
    wrapped.push("--".to_owned());
    wrapped.extend(command);
    (wrapped, None, true)
}

fn agent_tmux_session_name(provider: &str, cwd: Option<&str>) -> String {
    let mut name = String::from("mtide-agent-");
    for value in [provider, cwd.unwrap_or("default")] {
        for character in value.chars() {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                name.push(character);
            } else {
                name.push('_');
            }
            if name.len() >= 72 {
                break;
            }
        }
        if name.len() < 72 {
            name.push('_');
        }
        if name.len() >= 72 {
            break;
        }
    }
    name.truncate(72);
    name.trim_end_matches('_').to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AgentSession {
    #[serde(default = "agent_session_version")]
    version: u8,
    provider: String,
    input: String,
    screen: Vec<String>,
}

const AGENT_SESSION_VERSION: u8 = 1;
const AGENT_SESSION_MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const AGENT_SESSION_MAX_INPUT_BYTES: usize = 64 * 1024;
const AGENT_SESSION_MAX_LINES: usize = 2_000;
const AGENT_SESSION_MAX_LINE_BYTES: usize = 16 * 1024;

const fn agent_session_version() -> u8 {
    AGENT_SESSION_VERSION
}

fn editor_diff_markers(base: &[String], current: &[String]) -> Vec<Option<char>> {
    if base.len() > 2048 || current.len() > 2048 {
        return current.iter().map(|_| Some('+')).collect();
    }
    let mut lcs = vec![vec![0usize; current.len() + 1]; base.len() + 1];
    for row in (0..base.len()).rev() {
        for column in (0..current.len()).rev() {
            lcs[row][column] = if base[row] == current[column] {
                lcs[row + 1][column + 1] + 1
            } else {
                lcs[row + 1][column].max(lcs[row][column + 1])
            };
        }
    }
    let mut markers = Vec::with_capacity(current.len());
    let (mut base_index, mut current_index) = (0, 0);
    while current_index < current.len() {
        if base_index < base.len() && base[base_index] == current[current_index] {
            markers.push(None);
            base_index += 1;
            current_index += 1;
        } else if base_index < base.len()
            && (current_index == current.len()
                || lcs[base_index + 1][current_index] >= lcs[base_index][current_index + 1])
        {
            base_index += 1;
        } else {
            markers.push(Some('+'));
            current_index += 1;
        }
    }
    markers
}

fn builtin_commands() -> Vec<(String, String)> {
    [
        ("editor.save", "Save"),
        (
            "editor.formatExternal",
            "Format Document (External Formatter)",
        ),
        ("editor.reopenClosedTab", "Reopen Closed Tab"),
        ("editor.undo", "Undo"),
        ("editor.redo", "Redo"),
        ("editor.find", "Find"),
        ("editor.gotoLine", "Go to Line"),
        ("editor.scrollLeft", "Scroll Editor Left"),
        ("editor.scrollRight", "Scroll Editor Right"),
        ("lsp.documentSymbols", "Document Symbols"),
        ("lsp.workspaceSymbols", "Workspace Symbols (Find query)"),
        ("lsp.restart", "Restart Language Server"),
        ("editor.goToMatchingBracket", "Go to Matching Bracket"),
        ("editor.joinLines", "Join Lines"),
        ("editor.sortLines", "Sort Lines"),
        ("editor.uppercase", "Uppercase Lines"),
        ("editor.lowercase", "Lowercase Lines"),
        ("editor.splitSelectionLines", "Split Selection into Lines"),
        ("editor.duplicateLine", "Duplicate Line"),
        ("editor.deleteLine", "Delete Line"),
        ("editor.deleteWordLeft", "Delete Word Left"),
        ("editor.deleteWordRight", "Delete Word Right"),
        ("editor.toggleComment", "Toggle Comment"),
        ("view.toggleSidebar", "Toggle Sidebar"),
        ("view.toggleTerminal", "Toggle Terminal"),
        ("view.toggleAgent", "Toggle Agent"),
        ("view.switchTheme", "Switch Theme"),
        ("workspace.openFile", "Open File"),
        ("git.sync", "Sync Git (Pull then Push)"),
        ("workspace.saveAs", "Save Workspace As"),
        ("workspace.addFolder", "Add Folder to Workspace"),
        ("workspace.removeFolder", "Remove Active Workspace Folder"),
        ("settings.open", "Open Settings"),
        ("options.toggleDiffWordWrap", "Toggle Diff Word Wrap"),
        (
            "options.toggleDiffHighContrast",
            "Toggle Diff High Contrast",
        ),
        ("options.useChangesOnlyDiff", "Show Diff Changes Only"),
        ("options.useFullFileDiff", "Show Full File Diff"),
        ("options.useSplitDiff", "Use Split Diff View"),
        ("options.useUnifiedDiff", "Use Unified Diff View"),
        ("plugin.refreshRegistry", "Refresh Plugin Registry"),
    ]
    .into_iter()
    .map(|(id, title)| (id.to_owned(), title.to_owned()))
    .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn select_agent_provider(settings: &Settings, saved: Option<&AgentSession>) -> String {
    saved
        .filter(|session| settings.agent.providers.contains_key(&session.provider))
        .map(|session| session.provider.clone())
        .or_else(|| settings.agent.active.clone())
        .or_else(|| settings.agent.providers.keys().next().cloned())
        .unwrap_or_default()
}

fn agent_session_path() -> Option<PathBuf> {
    std::env::var_os("MTIDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("TTT_CONFIG_DIR"))
        .map(PathBuf::from)
        .map(|directory| directory.join("agent-session.json"))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|directory| directory.join(".mtide/agent-session.json"))
        })
}

fn load_agent_session(path: &Path) -> Option<AgentSession> {
    let contents = fs::read_to_string(path).ok()?;
    if contents.len() > AGENT_SESSION_MAX_FILE_BYTES {
        return None;
    }
    let mut session: AgentSession = serde_json::from_str(&contents).ok()?;
    if session.version != AGENT_SESSION_VERSION
        || session.provider.is_empty()
        || session.input.len() > AGENT_SESSION_MAX_INPUT_BYTES
        || session.screen.len() > AGENT_SESSION_MAX_LINES
        || session
            .screen
            .iter()
            .any(|line| line.len() > AGENT_SESSION_MAX_LINE_BYTES)
    {
        return None;
    }
    session.input = session.input.replace('\0', "");
    Some(session)
}

fn save_agent_session(path: &Path, session: &AgentSession) -> AppResult<()> {
    if session.version != AGENT_SESSION_VERSION
        || session.provider.is_empty()
        || session.input.len() > AGENT_SESSION_MAX_INPUT_BYTES
        || session.screen.len() > AGENT_SESSION_MAX_LINES
        || session
            .screen
            .iter()
            .any(|line| line.len() > AGENT_SESSION_MAX_LINE_BYTES)
    {
        return Err(crate::error::AppError::Workspace(
            "agent session exceeds persistence limits".to_owned(),
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(session)
        .map_err(|error| crate::error::AppError::Workspace(error.to_string()))?;
    if contents.len() > AGENT_SESSION_MAX_FILE_BYTES {
        return Err(crate::error::AppError::Workspace(
            "agent session file exceeds persistence limits".to_owned(),
        ));
    }
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&temporary, contents)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn language_overrides_path(workspace_root: &Path) -> Option<PathBuf> {
    Some(workspace_root.join(".mtide/language-overrides.json"))
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn load_language_overrides(path: &Path) -> Option<BTreeMap<String, String>> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn save_language_overrides(path: &Path, overrides: &BTreeMap<String, String>) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(overrides)
        .map_err(|error| crate::error::AppError::Workspace(error.to_string()))?;
    fs::write(path, contents)?;
    Ok(())
}

fn summarize_lsp_result(method: &str, result: &serde_json::Value) -> String {
    match method {
        "textDocument/completion" => {
            let items = result
                .get("items")
                .or_else(|| result.as_array().map(|_| result))
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            format!("LSP completion: {items} item(s)")
        }
        "textDocument/hover" => {
            if result.is_null() {
                "LSP hover: no information".to_owned()
            } else {
                "LSP hover: information available".to_owned()
            }
        }
        "textDocument/definition"
        | "textDocument/implementation"
        | "textDocument/typeDefinition"
        | "textDocument/references" => {
            let count = result.as_array().map_or(1, Vec::len);
            format!("LSP navigation: {count} location(s)")
        }
        _ => format!("LSP response: {}", json_value_kind(result)),
    }
}

fn diff_change_blocks(old: &[String], new: &[String], file_path: String) -> Vec<PluginDiffHunk> {
    let mut hunks = Vec::new();
    let mut old_index = 0;
    let mut new_index = 0;
    while old_index < old.len() || new_index < new.len() {
        if old_index < old.len() && new_index < new.len() && old[old_index] == new[new_index] {
            old_index += 1;
            new_index += 1;
            continue;
        }
        let start = new_index;
        let old_start = old_index;
        while old_index < old.len() || new_index < new.len() {
            if old_index < old.len() && new_index < new.len() && old[old_index] == new[new_index] {
                break;
            }
            if old_index < old.len() {
                old_index += 1;
            }
            if new_index < new.len() {
                new_index += 1;
            }
        }
        let kind = match (old_index > old_start, new_index > start) {
            (false, true) => "added",
            (true, false) => "deleted",
            _ => "modified",
        };
        hunks.push(PluginDiffHunk {
            file_path: file_path.clone(),
            kind: kind.to_owned(),
            line: start.min(new.len().saturating_sub(1)),
        });
    }
    hunks
}

fn lsp_execute_command_params(command: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(command) = command.as_str() {
        return Some(json!({"command": command, "arguments": []}));
    }
    let object = command.as_object()?;
    let command_name = object.get("command")?.as_str()?;
    Some(json!({
        "command": command_name,
        "arguments": object.get("arguments").cloned().unwrap_or_else(|| json!([])),
    }))
}

fn completion_items(result: &serde_json::Value) -> Vec<LspCompletionItem> {
    result
        .get("items")
        .and_then(serde_json::Value::as_array)
        .or_else(|| result.as_array())
        .map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|item| {
                    let label = item.get("label")?.as_str()?.to_owned();
                    let edit_range = item.get("textEdit").and_then(completion_text_edit_range);
                    let additional_edits = item
                        .get("additionalTextEdits")
                        .and_then(serde_json::Value::as_array)
                        .map(|edits| {
                            edits
                                .iter()
                                .filter_map(|edit| {
                                    Some(LspCompletionTextEdit {
                                        range: completion_range(edit.get("range")?)?,
                                        new_text: edit.get("newText")?.as_str()?.to_owned(),
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let commit_characters = item
                        .get("commitCharacters")
                        .and_then(serde_json::Value::as_array)
                        .map(|characters| {
                            characters
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .filter_map(|value| {
                                    let mut chars = value.chars();
                                    let character = chars.next()?;
                                    chars.next().is_none().then_some(character)
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let insert_text = item
                        .get("insertText")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            item.get("textEdit")
                                .and_then(|edit| edit.get("newText"))
                                .and_then(serde_json::Value::as_str)
                        })
                        .unwrap_or(&label)
                        .to_owned();
                    let insert_text = if item
                        .get("insertTextFormat")
                        .and_then(|value| value.as_u64())
                        == Some(2)
                    {
                        expand_lsp_snippet(&insert_text)
                    } else {
                        insert_text
                    };
                    let resolve_params = item.get("data").map(|_| item.clone());
                    Some(LspCompletionItem {
                        label,
                        insert_text,
                        edit_range,
                        additional_edits,
                        commit_characters,
                        resolve_params,
                    })
                })
                .take(20)
                .collect()
        })
}

fn completion_range(range: &serde_json::Value) -> Option<LspCompletionRange> {
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(LspCompletionRange {
        start_line: start.get("line")?.as_u64()? as usize,
        start_character: start.get("character")?.as_u64()? as usize,
        end_line: end.get("line")?.as_u64()? as usize,
        end_character: end.get("character")?.as_u64()? as usize,
    })
}

fn completion_text_edit_range(edit: &serde_json::Value) -> Option<LspCompletionRange> {
    edit.get("range")
        .or_else(|| edit.get("replace"))
        .or_else(|| edit.get("insert"))
        .and_then(completion_range)
}

fn merge_completion_item(
    original: &serde_json::Value,
    resolved: &serde_json::Value,
) -> serde_json::Value {
    let (Some(original), Some(resolved)) = (original.as_object(), resolved.as_object()) else {
        return resolved.clone();
    };
    let mut merged = original.clone();
    for (key, value) in resolved {
        merged.insert(key.clone(), value.clone());
    }
    serde_json::Value::Object(merged)
}

fn expand_lsp_snippet(snippet: &str) -> String {
    let chars = snippet.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(snippet.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' {
            if let Some(next) = chars.get(index + 1) {
                output.push(*next);
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if chars[index] != '$' {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        if chars.get(index + 1) == Some(&'{') {
            let Some(close) = chars[index + 2..]
                .iter()
                .position(|character| *character == '}')
            else {
                output.push('$');
                index += 1;
                continue;
            };
            let close = index + 2 + close;
            let body = chars[index + 2..close].iter().collect::<String>();
            let digit_end = body
                .char_indices()
                .find_map(|(offset, character)| (!character.is_ascii_digit()).then_some(offset))
                .unwrap_or(body.len());
            if digit_end > 0 {
                let default = &body[digit_end..];
                if let Some(default) = default.strip_prefix(':') {
                    output.push_str(default);
                } else if let Some(choices) = default.strip_prefix('|') {
                    if let Some(first) = choices
                        .split('|')
                        .next()
                        .and_then(|value| value.split(',').next())
                    {
                        output.push_str(first);
                    }
                }
            }
            index = close + 1;
            continue;
        }
        if chars
            .get(index + 1)
            .is_some_and(|character| character.is_ascii_digit())
        {
            index += 2;
            while chars
                .get(index)
                .is_some_and(|character| character.is_ascii_digit())
            {
                index += 1;
            }
            continue;
        }
        output.push('$');
        index += 1;
    }
    output
}

fn hover_text(result: &serde_json::Value) -> String {
    let contents = result.get("contents").unwrap_or(result);
    if let Some(text) = contents.as_str() {
        return text.to_owned();
    }
    if let Some(array) = contents.as_array() {
        return array
            .iter()
            .filter_map(|item| {
                item.as_str().map(str::to_owned).or_else(|| {
                    item.get("value")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    contents.to_string()
}

fn signature_help_text(result: &serde_json::Value) -> String {
    let Some(signatures) = result
        .get("signatures")
        .and_then(serde_json::Value::as_array)
    else {
        return String::new();
    };
    let active = result
        .get("activeSignature")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let Some(signature) = signatures.get(active).or_else(|| signatures.first()) else {
        return String::new();
    };
    let label = signature
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let documentation = signature
        .get("documentation")
        .and_then(|value| {
            value.as_str().map(str::to_owned).or_else(|| {
                value
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
        })
        .unwrap_or_default();
    let active_parameter = result
        .get("activeParameter")
        .and_then(serde_json::Value::as_u64)
        .map(|index| format!("Active parameter: {}", index + 1));
    [
        (!label.is_empty()).then_some(label.to_owned()),
        active_parameter,
        (!documentation.is_empty()).then_some(documentation),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n")
}

#[cfg(test)]
fn definition_location(result: &serde_json::Value) -> Option<(PathBuf, usize, usize)> {
    definition_locations(result).into_iter().next()
}

fn definition_locations(result: &serde_json::Value) -> Vec<(PathBuf, usize, usize)> {
    let values = result
        .as_array()
        .map_or_else(|| vec![result], |items| items.iter().collect());
    values
        .into_iter()
        .filter_map(|location| {
            let uri = location.get("uri")?.as_str()?.strip_prefix("file://")?;
            let start = location.get("range")?.get("start")?;
            let path = PathBuf::from(uri);
            let line = start.get("line")?.as_u64()? as usize;
            let character = start.get("character")?.as_u64()? as usize;
            let column = fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.lines().nth(line).map(str::to_owned))
                .and_then(|text| editor_column_from_utf16(&text, character))
                .unwrap_or(character);
            Some((path, line, column))
        })
        .collect()
}

fn document_symbols(
    result: &serde_json::Value,
    default_path: &Path,
) -> Vec<(String, PathBuf, usize, usize)> {
    fn visit(
        value: &serde_json::Value,
        default_path: &Path,
        symbols: &mut Vec<(String, PathBuf, usize, usize)>,
    ) {
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let (path, range) = if let Some(location) = value.get("location") {
            (
                location
                    .get("uri")
                    .and_then(serde_json::Value::as_str)
                    .and_then(uri_to_path)
                    .unwrap_or_else(|| default_path.to_path_buf()),
                location.get("range"),
            )
        } else {
            (default_path.to_path_buf(), value.get("range"))
        };
        if !name.is_empty() {
            if let Some(start) = range.and_then(|range| range.get("start")) {
                if let (Some(line), Some(character)) = (
                    start.get("line").and_then(serde_json::Value::as_u64),
                    start.get("character").and_then(serde_json::Value::as_u64),
                ) {
                    let line = line as usize;
                    let character = character as usize;
                    let column = fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| text.lines().nth(line).map(str::to_owned))
                        .and_then(|text| editor_column_from_utf16(&text, character))
                        .unwrap_or(character);
                    symbols.push((name.to_owned(), path, line, column));
                }
            }
        }
        if let Some(children) = value.get("children").and_then(serde_json::Value::as_array) {
            for child in children {
                visit(child, default_path, symbols);
            }
        }
    }

    let mut symbols = Vec::new();
    if let Some(values) = result.as_array() {
        for value in values {
            visit(value, default_path, &mut symbols);
        }
    } else {
        visit(result, default_path, &mut symbols);
    }
    symbols
}

fn workspace_edit_summary(edit: &serde_json::Value) -> Vec<String> {
    let mut summary = std::collections::BTreeMap::<String, usize>::new();
    if let Some(changes) = edit.get("changes").and_then(serde_json::Value::as_object) {
        for (uri, edits) in changes {
            let count = edits.as_array().map_or(0, Vec::len);
            if count > 0 {
                let label =
                    uri_to_path(uri).map_or_else(|| uri.clone(), |path| path.display().to_string());
                summary.insert(label, count);
            }
        }
    }
    if let Some(document_changes) = edit
        .get("documentChanges")
        .and_then(serde_json::Value::as_array)
    {
        for change in document_changes {
            let Some(uri) = change
                .get("textDocument")
                .and_then(|document| document.get("uri"))
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let count = change
                .get("edits")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            if count > 0 {
                let label = uri_to_path(uri)
                    .map_or_else(|| uri.to_owned(), |path| path.display().to_string());
                *summary.entry(label).or_default() += count;
            }
        }
    }
    let mut result: Vec<String> = summary
        .into_iter()
        .map(|(path, count)| format!("{count:>3} edit(s)  {path}"))
        .collect();
    for conflict in workspace_edit_resource_conflicts(edit) {
        result.push(format!("! CONFLICT  {conflict}"));
    }
    result
}

fn workspace_edit_resource_conflicts(edit: &serde_json::Value) -> Vec<String> {
    let Some(changes) = edit
        .get("documentChanges")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let mut conflicts = Vec::new();
    for change in changes {
        let options = change.get("options");
        let overwrite = options
            .and_then(|value| value.get("overwrite"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        match change.get("kind").and_then(serde_json::Value::as_str) {
            Some("create") | Some("rename") => {
                let target = change
                    .get("uri")
                    .or_else(|| change.get("newUri"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(uri_to_path);
                if let Some(path) = target.filter(|path| path.exists() && !overwrite) {
                    conflicts.push(format!(
                        "resource target already exists: {}",
                        path.display()
                    ));
                }
                if change.get("kind").and_then(serde_json::Value::as_str) == Some("rename") {
                    let source = change
                        .get("oldUri")
                        .and_then(serde_json::Value::as_str)
                        .and_then(uri_to_path);
                    if let Some(path) = source.filter(|path| !path.exists()) {
                        conflicts.push(format!("rename source does not exist: {}", path.display()));
                    }
                }
            }
            Some("delete") => {
                let ignore_if_missing = options
                    .and_then(|value| value.get("ignoreIfNotExists"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let path = change
                    .get("uri")
                    .and_then(serde_json::Value::as_str)
                    .and_then(uri_to_path);
                if let Some(path) = path.filter(|path| !path.exists() && !ignore_if_missing) {
                    conflicts.push(format!("delete target does not exist: {}", path.display()));
                }
            }
            _ => {}
        }
    }
    conflicts
}

fn path_to_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file://");
    if !path.starts_with('/') {
        uri.push('/');
    }
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            uri.push(*byte as char);
        } else {
            uri.push('%');
            uri.push(hex_digit(byte >> 4));
            uri.push(hex_digit(byte & 0x0f));
        }
    }
    uri
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (authority, path) = rest.split_once('/')?;
        if !authority.is_empty() && authority != "localhost" {
            return None;
        }
        format!("/{path}")
    };
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            let high = hex_value(high)?;
            let low = hex_value(low)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    #[cfg(windows)]
    let path = if path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') {
        path[1..].to_owned()
    } else {
        path
    };
    (!path.chars().any(char::is_control)).then(|| PathBuf::from(path))
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod file_uri_tests {
    use super::{path_to_uri, uri_to_path};
    use std::path::Path;

    #[test]
    fn file_uri_round_trips_spaces_unicode_and_percent_signs() {
        let path = Path::new("/tmp/mtide project/配置 100%.rs");
        let uri = path_to_uri(path);
        assert_eq!(
            uri,
            "file:///tmp/mtide%20project/%E9%85%8D%E7%BD%AE%20100%25.rs"
        );
        assert_eq!(uri_to_path(&uri), Some(path.to_path_buf()));
    }

    #[test]
    fn file_uri_rejects_remote_authorities_and_malformed_escapes() {
        assert_eq!(uri_to_path("file://example.com/tmp/file.rs"), None);
        assert_eq!(uri_to_path("file:///tmp/%ZZ"), None);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LspTextEdit {
    start: Position,
    end: Position,
    new_text: String,
}

fn parse_lsp_text_edits(result: &serde_json::Value, uri: &str) -> AppResult<Vec<LspTextEdit>> {
    let edits = if let Some(edits) = result.as_array() {
        edits
    } else {
        result
            .get("changes")
            .and_then(serde_json::Value::as_object)
            .and_then(|changes| changes.get(uri))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                crate::error::AppError::Lsp(format!("invalid text edit list for {uri}"))
            })?
    };
    edits
        .iter()
        .map(|edit| {
            let range = edit
                .get("range")
                .ok_or_else(|| crate::error::AppError::Lsp("text edit has no range".to_owned()))?;
            let start = parse_lsp_position(range.get("start"), "start")?;
            let end = parse_lsp_position(range.get("end"), "end")?;
            let new_text = edit
                .get("newText")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| crate::error::AppError::Lsp("text edit has no newText".to_owned()))?
                .to_owned();
            Ok(LspTextEdit {
                start,
                end,
                new_text,
            })
        })
        .collect()
}

fn parse_lsp_position(value: Option<&serde_json::Value>, label: &str) -> AppResult<Position> {
    let value = value
        .ok_or_else(|| crate::error::AppError::Lsp(format!("text edit has no {label} position")))?;
    Ok(Position {
        line: value
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| crate::error::AppError::Lsp(format!("invalid {label} line")))?
            as usize,
        column: value
            .get("character")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| crate::error::AppError::Lsp(format!("invalid {label} character")))?
            as usize,
    })
}

fn utf16_column(line: &str, character_column: usize) -> usize {
    line.chars()
        .take(character_column)
        .map(char::len_utf16)
        .sum()
}

fn editor_column_from_utf16(line: &str, utf16_column: usize) -> Option<usize> {
    let mut units = 0;
    for (column, character) in line.chars().enumerate() {
        if units == utf16_column {
            return Some(column);
        }
        units += character.len_utf16();
        if units > utf16_column {
            return None;
        }
    }
    (units == utf16_column).then_some(line.chars().count())
}

fn normalize_lsp_text_edits(
    buffer: &Buffer,
    uri: &str,
    edits: &mut [LspTextEdit],
) -> AppResult<()> {
    for edit in edits {
        for (label, position) in [("start", &mut edit.start), ("end", &mut edit.end)] {
            let Some(line) = buffer.lines().get(position.line) else {
                return Err(crate::error::AppError::Lsp(format!(
                    "text edit {label} is outside {uri}"
                )));
            };
            position.column = editor_column_from_utf16(line, position.column).ok_or_else(|| {
                crate::error::AppError::Lsp(format!(
                    "text edit {label} UTF-16 column is invalid in {uri}"
                ))
            })?;
        }
    }
    Ok(())
}

fn validate_lsp_text_edits(buffer: &Buffer, uri: &str, edits: &[LspTextEdit]) -> AppResult<()> {
    let mut ordered = edits.to_vec();
    ordered.sort_by_key(|edit| {
        (
            edit.start.line,
            edit.start.column,
            edit.end.line,
            edit.end.column,
        )
    });
    for edit in &ordered {
        let Some(start_line) = buffer.lines().get(edit.start.line) else {
            return Err(crate::error::AppError::Lsp(format!(
                "text edit start is outside {uri}"
            )));
        };
        let Some(end_line) = buffer.lines().get(edit.end.line) else {
            return Err(crate::error::AppError::Lsp(format!(
                "text edit end is outside {uri}"
            )));
        };
        if edit.start.column > start_line.chars().count()
            || edit.end.column > end_line.chars().count()
            || (edit.start.line, edit.start.column) > (edit.end.line, edit.end.column)
        {
            return Err(crate::error::AppError::Lsp(format!(
                "text edit range is invalid in {uri}"
            )));
        }
    }
    for pair in ordered.windows(2) {
        if (pair[1].start.line, pair[1].start.column) < (pair[0].end.line, pair[0].end.column) {
            return Err(crate::error::AppError::Lsp(format!(
                "overlapping text edits rejected in {uri}"
            )));
        }
    }
    Ok(())
}

fn apply_parsed_lsp_text_edits(buffer: &mut Buffer, edits: &[LspTextEdit]) -> usize {
    let edits: Vec<_> = edits
        .iter()
        .map(|edit| (edit.start, edit.end, edit.new_text.clone()))
        .collect();
    buffer.apply_text_edits(&edits)
}

fn apply_lsp_text_edits(buffer: &mut Buffer, uri: &str, result: &serde_json::Value) -> usize {
    let Ok(mut edits) = parse_lsp_text_edits(result, uri) else {
        return 0;
    };
    if normalize_lsp_text_edits(buffer, uri, &mut edits).is_err() {
        return 0;
    }
    if validate_lsp_text_edits(buffer, uri, &edits).is_err() {
        return 0;
    }
    apply_parsed_lsp_text_edits(buffer, &edits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetSpec {
    path: PathBuf,
    line: usize,
    column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestTarget {
    repository: String,
    number: u64,
}

fn load_plugin_runtime_for_workspace(
    package: &PluginPackage,
    settings_path: Option<&Path>,
    workspace_roots: &[PathBuf],
) -> AppResult<PluginRuntime> {
    let runtime = PluginRuntime::load_with_settings_path_and_workspace(
        package,
        settings_path,
        workspace_roots.first().map(PathBuf::as_path),
    )?;
    for root in workspace_roots.iter().skip(1) {
        runtime.add_workspace_root(root);
    }
    Ok(runtime)
}

#[allow(dead_code)]
fn parse_github_pull_request_url(value: &str) -> Option<PullRequestTarget> {
    let rest = value
        .strip_prefix("https://github.com/")
        .or_else(|| value.strip_prefix("http://github.com/"))?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let owner = parts.next()?.trim();
    let repository = parts.next()?.trim_end_matches(".git").trim();
    if owner.is_empty() || repository.is_empty() || parts.next()? != "pull" {
        return None;
    }
    let number = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(PullRequestTarget {
        repository: format!("https://github.com/{owner}/{repository}.git"),
        number,
    })
}

fn commit_detail_source_path(root: &Path, text: &str, line_index: usize) -> Option<PathBuf> {
    let relative = text
        .lines()
        .take(line_index.saturating_add(1))
        .filter_map(|line| {
            line.strip_prefix("+++ b/").or_else(|| {
                line.strip_prefix("diff --git a/")
                    .and_then(|paths| paths.split_once(" b/").map(|(path, _)| path))
            })
        })
        .last()?;
    (relative != "/dev/null").then(|| root.join(relative))
}

fn editor_link_at(line: &str, column: usize) -> Option<String> {
    let byte_column = line
        .char_indices()
        .nth(column)
        .map_or(line.len(), |(byte, _)| byte);
    let mut search_from = 0;
    for token in line.split_whitespace() {
        let Some(relative_start) = line[search_from..].find(token) else {
            break;
        };
        let start = search_from + relative_start;
        let end = start + token.len();
        search_from = end;
        let trim_boundary = |character: char| {
            matches!(
                character,
                '<' | '>'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | ','
                    | ';'
                    | '!'
                    | '?'
                    | '"'
                    | '\''
                    | '`'
            )
        };
        let candidate = token
            .trim_start_matches(trim_boundary)
            .trim_end_matches(|character| trim_boundary(character) || character == '.')
            .split(['，', '。', '；', '：', '！', '？', '、'])
            .next()
            .unwrap_or_default();
        if candidate.is_empty() {
            continue;
        }
        let left_trimmed = token.len() - token.trim_start_matches(trim_boundary).len();
        let candidate_start = start + left_trimmed;
        let candidate_end = candidate_start + candidate.len();
        if byte_column < candidate_start || byte_column >= candidate_end {
            continue;
        }
        let candidate = candidate.trim_end_matches(':');
        if candidate.starts_with("https://") || candidate.starts_with("http://") {
            return Some(candidate.to_owned());
        }
        if looks_like_file_reference(candidate) {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn looks_like_file_reference(reference: &str) -> bool {
    let Some(target) = parse_editor_file_reference(reference) else {
        return false;
    };
    let file_name = target.path.file_name().and_then(|name| name.to_str());
    target.line > 0
        || target.column > 0
        || target.path.components().count() > 1
        || file_name.is_some_and(|name| name.starts_with('.') || name.contains('.'))
}

fn parse_editor_file_reference(reference: &str) -> Option<TargetSpec> {
    let mut parts = reference.rsplitn(3, ':');
    let (Some(last), Some(previous)) = (parts.next(), parts.next()) else {
        return None;
    };
    if let (Ok(column), Ok(line), Some(path)) = (
        last.parse::<usize>(),
        previous.parse::<usize>(),
        parts.next(),
    ) {
        if line > 0 && !path.is_empty() {
            return Some(TargetSpec {
                path: PathBuf::from(path),
                line: line - 1,
                column: column.saturating_sub(1),
            });
        }
    }
    if let Some((path, line)) = reference.rsplit_once(':') {
        if let Ok(line) = line.parse::<usize>() {
            if line > 0 && !path.is_empty() {
                return Some(TargetSpec {
                    path: PathBuf::from(path),
                    line: line - 1,
                    column: 0,
                });
            }
        }
    }
    (!reference.is_empty()).then(|| TargetSpec {
        path: PathBuf::from(reference),
        line: 0,
        column: 0,
    })
}

fn valid_http_url(url: &str) -> bool {
    let Some((scheme, remainder)) = url.split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        || url.chars().any(char::is_control)
        || url.chars().any(char::is_whitespace)
    {
        return false;
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    if let Some(ipv6) = authority.strip_prefix('[') {
        let Some((host, suffix)) = ipv6.split_once(']') else {
            return false;
        };
        !host.is_empty()
            && (suffix.is_empty()
                || suffix.strip_prefix(':').is_some_and(|port| {
                    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
                }))
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        !host.is_empty()
            && port.is_none_or(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
    }
}

fn plugin_text_position(
    text: &str,
    width: usize,
    target_visual_row: usize,
    target_visual_column: usize,
) -> Option<(usize, usize)> {
    let width = width.max(1);
    let mut visual_row = 0;
    for (line_index, line) in text.lines().enumerate() {
        let mut visual_column = 0;
        let mut row_offset = 0;
        for (character_index, character) in line.chars().enumerate() {
            let character_width = if character == '\t' {
                4 - visual_column % 4
            } else {
                UnicodeWidthChar::width(character).unwrap_or(0).max(1)
            };
            if visual_column > 0 && visual_column + character_width > width {
                row_offset += 1;
                visual_column = 0;
            }
            if visual_row + row_offset == target_visual_row
                && target_visual_column < visual_column + character_width
            {
                return Some((line_index, character_index));
            }
            visual_column += character_width;
            if visual_column >= width {
                row_offset += 1;
                visual_column = 0;
            }
        }
        let row_count = row_offset + usize::from(visual_column > 0);
        if line.is_empty() && visual_row == target_visual_row {
            return Some((line_index, 0));
        }
        visual_row += row_count.max(1);
    }
    None
}

fn char_column_at_display_column(text: &str, target: usize) -> Option<usize> {
    let mut display_column = 0;
    for (index, character) in text.chars().enumerate() {
        let width = if character == '\t' {
            4 - display_column % 4
        } else {
            UnicodeWidthChar::width(character).unwrap_or(0).max(1)
        };
        if target < display_column + width {
            return Some(index);
        }
        display_column += width;
    }
    None
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceFile {
    #[serde(default)]
    folders: Vec<WorkspaceFolder>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceFolder {
    path: PathBuf,
}

fn default_workspace_roots(settings: &Settings) -> AppResult<Vec<PathBuf>> {
    if settings.default_folders.is_empty() {
        return workspace_roots(None);
    }
    let mut roots = Vec::with_capacity(settings.default_folders.len());
    for folder in &settings.default_folders {
        let path = if folder == "~" {
            dirs_home().unwrap_or_else(|| PathBuf::from(folder))
        } else if let Some(relative) = folder.strip_prefix("~/") {
            dirs_home()
                .map(|home| home.join(relative))
                .unwrap_or_else(|| PathBuf::from(folder))
        } else {
            PathBuf::from(folder)
        };
        let path = path.canonicalize().map_err(|error| {
            crate::error::AppError::Workspace(format!("default folder {}: {error}", path.display()))
        })?;
        if !path.is_dir() {
            return Err(crate::error::AppError::Workspace(format!(
                "default folder is not a directory: {}",
                path.display()
            )));
        }
        roots.push(path);
    }
    Ok(roots)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn workspace_roots(target: Option<&TargetSpec>) -> AppResult<Vec<PathBuf>> {
    let Some(target) = target else {
        return Ok(vec![
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        ]);
    };
    if target.path.is_dir() {
        return Ok(vec![target.path.clone()]);
    }
    if target
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("ttt")
    {
        return Ok(vec![target
            .path
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from)]);
    }
    let contents = fs::read_to_string(&target.path)?;
    let workspace: WorkspaceFile = serde_json::from_str(&contents)
        .map_err(|error| crate::error::AppError::Workspace(error.to_string()))?;
    let base = target.path.parent().unwrap_or_else(|| Path::new("."));
    let mut roots = Vec::with_capacity(workspace.folders.len());
    for folder in workspace.folders {
        let path = if folder.path.is_absolute() {
            folder.path
        } else {
            base.join(folder.path)
        };
        let path = path
            .canonicalize()
            .map_err(|error| crate::error::AppError::Workspace(error.to_string()))?;
        if !path.is_dir() {
            return Err(crate::error::AppError::Workspace(format!(
                "workspace folder is not a directory: {}",
                path.display()
            )));
        }
        roots.push(path);
    }
    if roots.is_empty() {
        return Err(crate::error::AppError::Workspace(
            "no workspace folders".to_owned(),
        ));
    }
    Ok(roots)
}

fn parse_new_hunk_line(header: &str) -> Option<u32> {
    let plus = header
        .split_whitespace()
        .find(|part| part.starts_with('+'))?;
    plus.trim_start_matches('+')
        .split(',')
        .next()?
        .parse::<u32>()
        .ok()
}

fn parse_location_target(target: &str) -> TargetSpec {
    let mut parts = target.rsplitn(3, ':');
    let (Some(column), Some(line), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
        return TargetSpec {
            path: PathBuf::from(target),
            line: 0,
            column: 0,
        };
    };
    let (Ok(line), Ok(column)) = (line.parse::<usize>(), column.parse::<usize>()) else {
        return TargetSpec {
            path: PathBuf::from(target),
            line: 0,
            column: 0,
        };
    };
    TargetSpec {
        path: PathBuf::from(path),
        line: line.saturating_sub(1),
        column: column.saturating_sub(1),
    }
}

fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn display_column(text: &str, column: usize) -> usize {
    text.chars()
        .take(column)
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

fn visual_row_for_column(text: &str, column: usize, width: usize) -> usize {
    let mut row = 0;
    let mut visual_column = 0;
    for character in text.chars().take(column) {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if character_width > 0 && visual_column + character_width > width {
            row += 1;
            visual_column = 0;
        }
        visual_column += character_width;
        if visual_column == width {
            row += 1;
            visual_column = 0;
        }
    }
    row
}

#[derive(Debug, Default)]
struct MarkdownDetailsScanState {
    fence: Option<(u8, usize)>,
    inline_code_ticks: Option<usize>,
}

fn markdown_details_tags(line: &str, state: &mut MarkdownDetailsScanState) -> (usize, usize) {
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    let fence_character = bytes
        .first()
        .copied()
        .filter(|byte| matches!(byte, b'`' | b'~'));
    let fence_length = fence_character.map_or(0, |character| {
        bytes.iter().take_while(|byte| **byte == character).count()
    });
    if let Some((open_character, open_length)) = state.fence {
        if fence_character == Some(open_character)
            && fence_length >= open_length
            && bytes[fence_length..]
                .iter()
                .all(|byte| matches!(byte, b' ' | b'\t'))
        {
            state.fence = None;
        }
        return (0, 0);
    }
    if fence_length >= 3
        && !(fence_character == Some(b'`') && bytes[fence_length..].contains(&b'`'))
    {
        state.fence = Some((fence_character.expect("fence character"), fence_length));
        state.inline_code_ticks = None;
        return (0, 0);
    }

    let bytes = line.as_bytes();
    let mut opens = 0;
    let mut closes = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            let start = index;
            while index < bytes.len() && bytes[index] == b'`' {
                index += 1;
            }
            let run_length = index - start;
            if let Some(delimiter_length) = state.inline_code_ticks {
                if run_length == delimiter_length {
                    state.inline_code_ticks = None;
                }
            } else {
                let escaped = bytes[..start]
                    .iter()
                    .rev()
                    .take_while(|byte| **byte == b'\\')
                    .count()
                    % 2
                    == 1;
                if !escaped {
                    state.inline_code_ticks = Some(run_length);
                }
            }
            continue;
        }
        if state.inline_code_ticks.is_none() {
            if bytes
                .get(index..)
                .is_some_and(|tail| tail.starts_with(b"</details>"))
            {
                closes += 1;
                index += "</details>".len();
                continue;
            }
            if bytes
                .get(index..)
                .is_some_and(|tail| tail.starts_with(b"<details"))
                && bytes
                    .get(index + "<details".len())
                    .is_some_and(|byte| matches!(byte, 62 | 32 | 9 | 47))
            {
                opens += 1;
                index += "<details".len();
                continue;
            }
        }
        index += 1;
    }
    (opens, closes)
}

fn wrapped_row_count(text: &str, width: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    let mut rows = 1;
    let mut visual_column = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if character_width > 0 && visual_column + character_width > width {
            rows += 1;
            visual_column = 0;
        }
        visual_column += character_width;
        if visual_column == width {
            rows += 1;
            visual_column = 0;
        }
    }
    rows
}

fn column_for_visual_position(
    text: &str,
    target_row: usize,
    target_column: usize,
    width: usize,
) -> usize {
    let mut row = 0;
    let mut visual_column = 0;
    for (index, character) in text.chars().enumerate() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if character_width > 0 && visual_column + character_width > width {
            row += 1;
            visual_column = 0;
        }
        if row == target_row && target_column <= visual_column {
            return index;
        }
        visual_column += character_width;
        if visual_column == width {
            row += 1;
            visual_column = 0;
        }
    }
    text.chars().count()
}

fn discover_plugin_packages(workspace_roots: &[PathBuf]) -> Vec<PluginPackage> {
    let mut packages = Vec::new();
    let mut names = BTreeSet::new();
    let mut plugin_directories = Vec::new();
    if let Some(config_directory) =
        std::env::var_os("MTIDE_CONFIG_DIR").or_else(|| std::env::var_os("TTT_CONFIG_DIR"))
    {
        plugin_directories.push(PathBuf::from(config_directory).join("plugins"));
    } else {
        let global_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".config"))
            });
        if let Some(config) = global_config {
            plugin_directories.push(config.join("ttt/plugins"));
        }
        if let Some(local_config) = std::env::current_dir().ok().map(|path| path.join(".mtide")) {
            plugin_directories.push(local_config.join("plugins"));
        }
    }
    for directory in plugin_directories {
        scan_plugin_directory(&directory, &mut packages, &mut names);
    }
    for root in workspace_roots {
        scan_plugin_directory(&root.join("plugins"), &mut packages, &mut names);
    }
    packages
}

fn config_root(settings_path: Option<&Path>) -> Option<PathBuf> {
    settings_path
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| {
            std::env::var_os("MTIDE_CONFIG_DIR")
                .or_else(|| std::env::var_os("TTT_CONFIG_DIR"))
                .map(PathBuf::from)
        })
        .or_else(|| std::env::current_dir().ok().map(|dir| dir.join(".mtide")))
}

fn scan_plugin_directory(
    directory: &Path,
    packages: &mut Vec<PluginPackage>,
    names: &mut BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let Ok(package) = PluginPackage::load(&path) else {
            continue;
        };
        let name = package.manifest.name.clone();
        if names.insert(name) {
            packages.push(package);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn settings_reload_loads_theme_generated_after_startup() {
        let root = std::env::temp_dir().join(format!(
            "mtide-theme-reload-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("themes")).expect("themes directory");
        let settings_path = root.join("settings.json");
        fs::write(&settings_path, r#"{"theme":"omarchy"}"#).expect("settings file");
        let mut app = App::new(&[
            "--config".to_owned(),
            settings_path.to_string_lossy().into_owned(),
            "--welcome".to_owned(),
        ])
        .expect("app");
        fs::write(
            root.join("themes/omarchy.json"),
            r##"{"default":{"fg":"#fafafa","bg":"#112233"},"borderActive":{"fg":"#abcdef"}}"##,
        )
        .expect("generated theme");

        app.reload_theme_from_settings();

        assert_eq!(app.theme.background, ratatui::style::Color::Rgb(17, 34, 51));
        assert_eq!(app.theme.accent, ratatui::style::Color::Rgb(171, 205, 239));
        assert_eq!(app.theme_name(), "omarchy");
        assert_eq!(app.status_message, "Theme reloaded: omarchy");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tab_cycles_visible_focus() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("key");
        assert_eq!(app.focus(), Focus::Terminal);
    }

    #[test]
    fn plugin_directory_scan_is_sorted_skips_invalid_and_deduplicates_names() {
        let root = std::env::temp_dir().join(format!("mtide-plugin-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let global = root.join("global");
        let workspace = root.join("workspace");
        fs::create_dir_all(global.join("plugins/zeta")).expect("global plugin");
        fs::create_dir_all(workspace.join("plugins/alpha")).expect("workspace plugin");
        fs::create_dir_all(workspace.join("plugins/zeta")).expect("duplicate plugin");
        fs::create_dir_all(workspace.join("plugins/broken")).expect("broken plugin");
        for path in [
            global.join("plugins/zeta"),
            workspace.join("plugins/alpha"),
            workspace.join("plugins/zeta"),
        ] {
            fs::write(
                path.join("plugin.ttt.json"),
                format!(
                    r#"{{"name":"{}","entry":"init.lua","api":2}}"#,
                    path.file_name().unwrap().to_string_lossy()
                ),
            )
            .expect("manifest");
            fs::write(path.join("init.lua"), "").expect("entry");
        }
        let mut packages = Vec::new();
        let mut names = BTreeSet::new();
        scan_plugin_directory(&global.join("plugins"), &mut packages, &mut names);
        scan_plugin_directory(&workspace.join("plugins"), &mut packages, &mut names);
        assert_eq!(
            packages
                .iter()
                .map(|package| package.manifest.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn hidden_panels_are_skipped() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .expect("key");
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("key");
        assert_eq!(app.focus(), Focus::Terminal);
    }

    #[test]
    fn plugin_flag_does_not_become_editor_target() {
        let app = App::new(&["--plugin".into(), "/tmp/missing-plugin".into()]);
        assert!(app.is_err());
    }

    #[test]
    fn exec_script_value_does_not_become_editor_target() {
        let app = App::new(&["--exec".into(), "[]".into()]).expect("exec app");
        assert_eq!(app.file_lines()[0], "# mtide");
    }

    #[test]
    fn workspace_option_value_does_not_become_editor_target() {
        let root = std::env::temp_dir().join(format!(
            "mtide-workspace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("workspace root");
        let workspace = root.join("project.ttt");
        std::fs::write(
            &workspace,
            format!(r#"{{"folders":[{{"path":"{}"}}]}}"#, root.display()),
        )
        .expect("workspace file");
        let app = App::new(&["--workspace".into(), workspace.to_string_lossy().into()])
            .expect("workspace app");
        assert_eq!(app.buffer().path(), None);
        assert_eq!(
            app.workspace_roots,
            vec![root.canonicalize().expect("canonical root")]
        );
    }

    #[test]
    fn default_folders_create_a_multi_root_workspace_without_an_explicit_target() {
        let root = std::env::temp_dir().join(format!(
            "mtide-default-folders-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let first = root.join("one");
        let second = root.join("two");
        std::fs::create_dir_all(&first).expect("first folder");
        std::fs::create_dir_all(&second).expect("second folder");
        let config = root.join("settings.json");
        std::fs::write(
            &config,
            format!(
                r#"{{"defaultFolders":["{}","{}"]}}"#,
                first.display(),
                second.display()
            ),
        )
        .expect("settings");
        let app = App::new(&["--config".into(), config.to_string_lossy().into()])
            .expect("default folders app");
        assert_eq!(app.buffer().path(), None);
        assert_eq!(
            app.workspace_roots,
            vec![
                first.canonicalize().expect("first canonical"),
                second.canonicalize().expect("second canonical")
            ]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn clear_default_folders_persists_to_explicit_config() {
        let root = std::env::temp_dir().join(format!(
            "mtide-clear-folders-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let folder = root.join("folder");
        std::fs::create_dir_all(&folder).expect("folder");
        let config = root.join("settings.json");
        std::fs::write(
            &config,
            format!(r#"{{"defaultFolders":["{}"]}}"#, folder.display()),
        )
        .expect("settings");
        let app = App::new(&[
            "--config".into(),
            config.to_string_lossy().into(),
            "--clear-default-folders".into(),
        ])
        .expect("cleared app");
        assert_eq!(
            app.workspace_roots,
            vec![std::env::current_dir().expect("cwd")]
        );
        let loaded = Settings::load_from_path(&config);
        assert!(loaded.default_folders.is_empty());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn summarizes_completion_response() {
        assert_eq!(
            summarize_lsp_result(
                "textDocument/completion",
                &serde_json::json!({"items":[{"label":"println!"}]})
            ),
            "LSP completion: 1 item(s)"
        );
    }

    #[test]
    fn exposes_current_line_diagnostic_message() {
        let path = std::env::temp_dir().join(format!("mtide-diagnostic-{}.rs", std::process::id()));
        std::fs::write(&path, "first\nsecond\nthird\n").expect("source");
        let app_path = path.to_string_lossy().into_owned();
        let mut app = App::new(&[app_path]).expect("app");
        let uri = format!("file://{}", path.to_string_lossy());
        assert!(app.lsp.apply_notification(&serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 2}},
                    "severity": 1,
                    "message": "example diagnostic"
                }]
            }
        })));
        assert_eq!(
            app.diagnostic_message(0).as_deref(),
            Some("example diagnostic")
        );
        assert_eq!(app.diagnostic_severity(1), Some(1));
        assert_eq!(
            app.diagnostic_message(2).as_deref(),
            Some("example diagnostic")
        );
        assert_eq!(app.problems().len(), 1);
        app.select_sidebar_tab_by_index(4);
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("jump to problem");
        assert_eq!(app.focus(), Focus::Editor);
        assert_eq!(app.cursor(), Position { line: 0, column: 0 });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn diagnostic_highlight_uses_utf16_range_columns() {
        let path =
            std::env::temp_dir().join(format!("mtide-diagnostic-range-{}.rs", std::process::id()));
        std::fs::write(&path, "😀abc\n").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        let uri = format!("file://{}", path.to_string_lossy());
        assert!(app.lsp.apply_notification(&serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 4}},
                    "severity": 1,
                    "message": "range"
                }]
            }
        })));
        assert!(!app.is_diagnostic_position(0, 0));
        assert!(app.is_diagnostic_position(0, 1));
        assert!(app.is_diagnostic_position(0, 2));
        assert!(!app.is_diagnostic_position(0, 3));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn diagnostics_without_severity_remain_visible_as_information() {
        let path = std::env::temp_dir().join(format!(
            "mtide-diagnostic-default-severity-{}.rs",
            std::process::id()
        ));
        std::fs::write(&path, "value\n").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        let uri = format!("file://{}", path.to_string_lossy());
        assert!(app.lsp.apply_notification(&serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                    "message": "informational"
                }]
            }
        })));
        assert_eq!(app.diagnostic_severity(0), Some(3));
        assert!(app.is_diagnostic_position(0, 2));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zero_width_and_invalid_utf16_diagnostic_ranges_do_not_mark_neighboring_chars() {
        let path = std::env::temp_dir().join(format!(
            "mtide-diagnostic-empty-range-{}.rs",
            std::process::id()
        ));
        std::fs::write(&path, "😀x\n").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        let uri = format!("file://{}", path.to_string_lossy());
        assert!(app.lsp.apply_notification(&serde_json::json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                    "severity": 2,
                    "message": "zero width"
                }, {
                    "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}},
                    "severity": 2,
                    "message": "inside surrogate pair"
                }]
            }
        })));
        assert!(!app.is_diagnostic_position(0, 0));
        assert!(!app.is_diagnostic_position(0, 1));
        assert_eq!(app.diagnostic_severity(0), Some(2));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parses_file_location_targets_without_breaking_plain_paths() {
        assert_eq!(
            parse_location_target("src/main.rs:12:7"),
            TargetSpec {
                path: PathBuf::from("src/main.rs"),
                line: 11,
                column: 6,
            }
        );
        assert_eq!(
            parse_location_target("src/main.rs"),
            TargetSpec {
                path: PathBuf::from("src/main.rs"),
                line: 0,
                column: 0,
            }
        );
    }

    #[test]
    fn parses_github_pull_request_targets() {
        assert_eq!(
            parse_github_pull_request_url("https://github.com/acme/editor/pull/42"),
            Some(PullRequestTarget {
                repository: "https://github.com/acme/editor.git".into(),
                number: 42,
            })
        );
        assert!(
            parse_github_pull_request_url("https://github.com/acme/editor/issues/42").is_none()
        );
        assert!(parse_github_pull_request_url("/tmp/project").is_none());
    }

    #[test]
    fn parses_new_file_line_from_diff_hunk_header() {
        assert_eq!(parse_new_hunk_line("@ -4,2 +9,3 @@ function"), Some(9));
        assert_eq!(parse_new_hunk_line("@ -1 +1 @@"), Some(1));
        assert_eq!(parse_new_hunk_line("not a hunk"), None);
    }

    #[test]
    fn git_diff_context_collapses_long_quiet_gaps_and_expands_in_place() {
        let mut app = App::new(&[]).expect("welcome app");
        app.change_diff = vec![
            DiffLine {
                kind: '@',
                text: "@@ -1,10 +1,10 @@".into(),
            },
            DiffLine {
                kind: ' ',
                text: "before".into(),
            },
            DiffLine {
                kind: ' ',
                text: "one".into(),
            },
            DiffLine {
                kind: ' ',
                text: "two".into(),
            },
            DiffLine {
                kind: ' ',
                text: "three".into(),
            },
            DiffLine {
                kind: ' ',
                text: "four".into(),
            },
            DiffLine {
                kind: ' ',
                text: "five".into(),
            },
            DiffLine {
                kind: ' ',
                text: "after".into(),
            },
        ];
        let collapsed = app.rendered_change_diff();
        assert_eq!(collapsed.len(), 6);
        assert_eq!(collapsed[3].kind, '~');
        app.output_scroll = 3;
        app.handle_output_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(app.rendered_change_diff().len(), 8);
    }

    #[test]
    fn pull_request_review_rejects_invalid_repository_before_mutating_view() {
        let mut app = App::new(&[]).expect("welcome app");
        let initial_diff = app.change_diff.clone();
        assert!(app.load_pull_request_review("", 1).is_err());
        assert_eq!(app.change_diff, initial_diff);
    }

    #[test]
    fn pull_request_comment_enter_jumps_to_file_line() {
        let mut app = App::new(&[]).expect("welcome app");
        app.pull_request_comments.push(PullRequestComment {
            id: 1,
            body: "review".into(),
            path: "src/main.rs".into(),
            line: Some(10),
            side: Some("RIGHT".into()),
            user: Some(crate::git::PullRequestCommentUser {
                login: "reviewer".into(),
            }),
        });
        app.pull_request_comments_visible = true;
        app.focus = Focus::Output;
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("jump comment");
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.cursor().line, 9);
    }

    #[test]
    fn horizontal_viewport_is_keyboard_scrollable() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_key(key(
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("horizontal scroll right");
        assert_eq!(app.horizontal_scroll(), 4);
        app.handle_key(key(
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("horizontal scroll left");
        assert_eq!(app.horizontal_scroll(), 0);
    }

    #[test]
    fn horizontal_scroll_commands_clamp_to_the_visible_line_and_respect_wrap() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["abcdefghijkl".into()], None);
        app.set_editor_viewport_width(6);

        assert!(app
            .execute_builtin_command("editor.scrollRight")
            .expect("scroll right"));
        assert_eq!(app.horizontal_scroll(), 4);
        app.execute_builtin_command("editor.scrollRight")
            .expect("scroll right to end");
        assert_eq!(app.horizontal_scroll(), 6);
        app.execute_builtin_command("editor.scrollRight")
            .expect("scroll right at end");
        assert_eq!(app.horizontal_scroll(), 6);
        app.buffers[0] = Buffer::new(vec!["a\tbcdef".into()], None);
        app.execute_builtin_command("editor.scrollRight")
            .expect("scroll tabbed line");
        assert_eq!(app.horizontal_scroll(), 3);
        app.execute_builtin_command("editor.scrollLeft")
            .expect("scroll left");
        assert_eq!(app.horizontal_scroll(), 0);

        app.settings.editor.word_wrap = true;
        app.execute_builtin_command("editor.scrollRight")
            .expect("wrapped editor ignores horizontal scrolling");
        assert_eq!(app.horizontal_scroll(), 0);
    }

    #[test]
    fn plugin_diff_tabs_support_scroll_and_hunk_navigation() {
        let mut app = App::new(&[]).expect("welcome app");
        app.plugin_custom_tab = Some((0, "changes".to_owned()));
        app.plugin_file_view = Some((
            "Changes".to_owned(),
            "diff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new\n@@ -8 +8 @@\n-old2\n+new2".to_owned(),
        ));
        app.handle_key(key(KeyCode::Char(']'), KeyModifiers::NONE))
            .expect("next hunk");
        assert_eq!(app.plugin_file_view_scroll(), 4);
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
            .expect("scroll up");
        assert_eq!(app.plugin_file_view_scroll(), 3);
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close plugin tab");
        assert!(app.plugin_file_view().is_none());
    }

    #[test]
    fn read_only_diff_links_hover_and_open_relative_to_source_file() {
        let root = std::env::temp_dir().join(format!("mtide-diff-link-{}", std::process::id()));
        let docs = root.join("docs");
        let source = docs.join("review.md");
        let target = root.join("src/lib.rs");
        std::fs::create_dir_all(&docs).expect("docs directory");
        std::fs::create_dir_all(target.parent().expect("target parent")).expect("target directory");
        std::fs::write(&source, "review context").expect("source file");
        std::fs::write(&target, "one\ntwo\n").expect("target file");
        let mut app = App::new(&[]).expect("app");
        let added_line = "+See ../src/lib.rs:2:2";
        app.git_diff_tab_text = Some(format!("diff --git a/x b/x\n@@ -1 +1 @@\n{added_line}"));
        app.plugin_file_view = Some((
            "Read-only diff".into(),
            app.git_diff_tab_text.clone().unwrap(),
        ));
        app.plugin_file_view_path = Some(source);
        let column = added_line
            .chars()
            .position(|character| character == 'l')
            .expect("link column");

        app.update_plugin_file_view_hover(2, column, 80, 24, true);
        assert_eq!(
            app.plugin_file_view_hover_link(2),
            Some("../src/lib.rs:2:2")
        );
        assert!(app.open_plugin_file_view_link_at(2, column, 80, 24));
        assert_eq!(app.buffer().path(), Some(target.as_path()));
        assert_eq!(app.cursor(), Position { line: 1, column: 1 });
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn plugin_diff_pointer_mapping_accounts_for_wrapped_wide_characters() {
        assert_eq!(plugin_text_position("A中B\nx", 3, 0, 2), Some((0, 1)));
        assert_eq!(plugin_text_position("A中B\nx", 3, 1, 0), Some((0, 2)));
        assert_eq!(plugin_text_position("A中B\nx", 3, 2, 0), Some((1, 0)));
        assert_eq!(plugin_text_position("\tlink", 6, 0, 4), Some((0, 1)));
        assert_eq!(char_column_at_display_column("A中B", 2), Some(1));
        assert_eq!(char_column_at_display_column("A中B", 3), Some(2));
    }

    #[test]
    fn current_changes_output_links_open_from_the_changed_file_directory() {
        let root = std::env::temp_dir().join(format!("mtide-output-link-{}", std::process::id()));
        let docs = root.join("docs");
        let target = root.join("src/lib.rs");
        std::fs::create_dir_all(&docs).expect("docs directory");
        std::fs::create_dir_all(target.parent().expect("target parent")).expect("target directory");
        std::fs::write(docs.join("review.md"), "review").expect("changed file");
        std::fs::write(&target, "one\ntwo\n").expect("target file");
        let mut app = App::new(&[]).expect("app");
        app.changes = vec![GitChange {
            index: ' ',
            worktree: 'M',
            path: "docs/review.md".into(),
        }];
        app.change_roots = vec![root.clone()];
        app.change_diff = vec![
            DiffLine {
                kind: '@',
                text: "@@ -1 +1 @@".into(),
            },
            DiffLine {
                kind: '+',
                text: "See ../src/lib.rs:2:2".into(),
            },
        ];
        let column = "+See ../src/lib.rs:2:2"
            .chars()
            .position(|character| character == 'l')
            .expect("link column");

        app.update_output_hover(1, column, true);
        assert_eq!(app.output_hover_link(1), Some("../src/lib.rs:2:2"));
        assert!(app.open_output_link_at(1, column));
        assert_eq!(app.buffer().path(), Some(target.as_path()));
        assert_eq!(app.cursor(), Position { line: 1, column: 1 });
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn commit_detail_output_links_resolve_from_each_patched_file() {
        let root =
            std::env::temp_dir().join(format!("mtide-commit-detail-link-{}", std::process::id()));
        let docs = root.join("docs");
        let target = root.join("src/lib.rs");
        std::fs::create_dir_all(&docs).expect("docs directory");
        std::fs::create_dir_all(target.parent().expect("target parent")).expect("target directory");
        std::fs::write(&target, "one\ntwo\n").expect("target file");
        let detail = "commit abcdef0123456789\n\n    update docs\n\ndiff --git a/docs/change.md b/docs/change.md\n--- a/docs/change.md\n+++ b/docs/change.md\n@@ -1 +1 @@\n+See ../src/lib.rs:2:2\n";
        let mut app = App::new(&[]).expect("app");
        app.history.push(GitCommit {
            hash: "abcdef0123456789".into(),
            author: "Ada".into(),
            date: "2026-11-05".into(),
            subject: "update docs".into(),
        });
        app.commit_detail = Some((app.history[0].clone(), root.clone(), detail.to_owned()));
        let line_index = detail
            .lines()
            .position(|line| line.contains("../src/lib.rs"))
            .expect("patch link line");
        let column = detail
            .lines()
            .nth(line_index)
            .and_then(|line| line.chars().position(|character| character == 'l'))
            .expect("link column");

        app.output_scroll = line_index;
        app.update_output_hover(0, column, true);
        assert_eq!(app.output_hover_link(line_index), Some("../src/lib.rs:2:2"));
        assert!(app.open_output_link_at(0, column));
        assert_eq!(app.buffer().path(), Some(target.as_path()));
        assert_eq!(app.cursor(), Position { line: 1, column: 1 });
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn pull_request_comment_output_links_open_workspace_file_at_comment_line() {
        let root = std::env::temp_dir().join(format!("mtide-comment-link-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("workspace source");
        let target = root.join("src/lib.rs");
        std::fs::write(&target, "one\ntwo\n").expect("target file");
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.pull_request_comments.push(PullRequestComment {
            id: 1,
            body: "review comment".into(),
            path: "src/lib.rs".into(),
            line: Some(2),
            side: Some("RIGHT".into()),
            user: Some(crate::git::PullRequestCommentUser {
                login: "reviewer".into(),
            }),
        });
        app.pull_request_comments_visible = true;
        app.change_diff.push(DiffLine {
            kind: '@',
            text: "@@ -1 +1 @@".into(),
        });
        let displayed = app.output_reference_lines().expect("comment output")[0].clone();
        let column = displayed
            .chars()
            .position(|character| character == 'l')
            .expect("path column");

        app.update_output_hover(0, column, true);
        assert_eq!(app.output_hover_link(0), Some("src/lib.rs:2"));
        assert!(app.open_output_link_at(0, column));
        assert_eq!(app.buffer().path(), Some(target.as_path()));
        assert_eq!(app.cursor(), Position { line: 1, column: 0 });
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn pull_request_review_switches_between_diff_and_comments() {
        let mut app = App::new(&[]).expect("welcome app");
        app.pull_request_target = Some(("https://github.com/acme/project".into(), 12));
        app.change_diff = vec![
            DiffLine {
                kind: '-',
                text: "old".into(),
            },
            DiffLine {
                kind: '+',
                text: "new".into(),
            },
        ];
        app.pull_request_comments.push(PullRequestComment {
            id: 1,
            body: "review note".into(),
            path: "src/lib.rs".into(),
            line: Some(3),
            side: Some("RIGHT".into()),
            user: None,
        });

        assert!(app.diff_view_active());
        assert!(app.output_reference_lines().expect("diff lines")[0].starts_with("-"));
        app.handle_output_key(key(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(app.pull_request_comments_visible());
        assert!(app.output_reference_lines().expect("comments")[0].contains("review note"));
        app.handle_output_key(key(KeyCode::Char('m'), KeyModifiers::NONE));
        assert!(!app.pull_request_comments_visible());
        assert!(app.diff_view_active());
        assert!(app.output_reference_lines().expect("diff again")[0].starts_with("-"));
        app.handle_output_key(key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.pull_request_comment_mode());
    }

    #[test]
    fn split_plugin_diff_preserves_sides() {
        let text = "diff --git a/a b/a\n-old\n+new\n context";
        let rendered = split_unified_diff_text(text);
        assert!(rendered.contains("-old"));
        assert!(rendered.contains("+new"));
        assert!(rendered.contains("context"));
        assert!(rendered.lines().all(|line| line.contains('│')));
    }

    #[test]
    fn automation_wait_for_parses_text_and_bounded_timeout() {
        let (needle, timeout) = automation_wait_for("ready state timeout=1200").expect("wait-for");
        assert_eq!(needle, "ready state");
        assert_eq!(timeout, Duration::from_millis(1200));
        let (_, default_timeout) = automation_wait_for("ready").expect("default timeout");
        assert_eq!(default_timeout, Duration::from_secs(5));
        assert!(automation_wait_for("ready timeout=30001").is_err());
        assert!(automation_wait_for("timeout=10").is_err());
    }

    #[test]
    fn collapses_selected_plugin_diff_hunk() {
        let text = "diff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new\n@@ -4 +4 @@\n-old2\n+new2";
        let mut collapsed = BTreeSet::new();
        collapsed.insert(1);
        let rendered = collapse_plugin_diff_hunks(text, &collapsed);
        assert!(rendered.contains("-old\n+new"));
        assert!(rendered.contains("… [hunk collapsed]"));
        assert!(!rendered.contains("-old2\n+new2"));
    }

    #[test]
    fn git_credential_prompt_keeps_password_in_memory_and_masks_focus_flow() {
        let mut app = App::new(&[]).expect("welcome app");
        app.git_credential_prompt = Some(GitCredentialPrompt {
            operation: "push".to_owned(),
            root: std::env::temp_dir(),
            username: String::new(),
            password: String::new(),
            username_cursor: 0,
            password_cursor: 0,
            password_focus: false,
        });
        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::NONE))
            .expect("username");
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("password focus");
        for character in "secret".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("password");
        }
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .expect("password cursor");
        app.handle_paste("X").expect("password paste");
        let prompt = app.git_credential_prompt().expect("prompt");
        assert_eq!(prompt.username, "u");
        assert_eq!(prompt.password, "secreXt");
        assert!(prompt.password_focus);
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel credentials");
        assert!(app.git_credential_prompt().is_none());
    }

    #[test]
    fn git_authentication_retry_keeps_the_originating_repository_root() {
        let mut app = App::new(&[]).expect("welcome app");
        let root = std::env::temp_dir().join(format!("mtide-git-auth-root-{}", std::process::id()));
        app.handle_git_remote_failure("push", &root, "authentication failed");
        let prompt = app.git_credential_prompt().expect("credential prompt");
        assert_eq!(prompt.root, root);
        assert_eq!(prompt.operation, "push");
    }

    #[test]
    fn git_remote_operations_are_reaped_by_the_background_poll() {
        let root = std::env::temp_dir().join(format!("mtide-git-worker-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("workspace");
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.run_git_remote("fetch");
        assert_eq!(app.git_remote_pending.as_deref(), Some("fetch"));
        for _ in 0..100 {
            app.poll_background();
            if app.git_remote_pending.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.git_remote_pending.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_remote_completion_does_not_refresh_a_different_active_workspace_root() {
        let base =
            std::env::temp_dir().join(format!("mtide-git-root-routing-{}", std::process::id()));
        let original_root = base.join("original");
        let active_root = base.join("active");
        std::fs::create_dir_all(&original_root).expect("original root");
        std::fs::create_dir_all(&active_root).expect("active root");
        let active_file = active_root.join("main.rs");
        std::fs::write(&active_file, "fn main() {}\n").expect("active file");
        let workspace = base.join("project.ttt");
        std::fs::write(
            &workspace,
            r#"{"folders":[{"path":"original"},{"path":"active"}]}"#,
        )
        .expect("workspace file");
        let mut app =
            App::new(&[workspace.to_string_lossy().into_owned()]).expect("multi-root app");
        app.open_buffer(Buffer::open(&active_file).expect("active buffer"));
        assert_eq!(app.active_git_root(), active_root);
        app.git_remote_pending = Some("fetch".to_owned());
        let generation = app.git_state_generation.load(Ordering::Acquire);
        app.git_remote_tx
            .send(GitRemoteCompletion {
                operation: "fetch".to_owned(),
                root: original_root.clone(),
                result: Ok(()),
            })
            .expect("completion receiver");
        app.poll_background();
        assert!(app.git_remote_pending.is_none());
        assert_eq!(app.git_state_generation.load(Ordering::Acquire), generation);
        assert!(app
            .status_message()
            .contains(&original_root.display().to_string()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn git_history_is_loaded_by_the_background_poll() {
        let root = std::env::temp_dir().join(format!(
            "mtide-git-history-worker-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).expect("workspace");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git command")
        };
        assert!(run(&["init", "-q"]).status.success());
        assert!(run(&["config", "user.email", "mtide@example.test"])
            .status
            .success());
        assert!(run(&["config", "user.name", "mtide-test"]).status.success());
        std::fs::write(root.join("README"), "history\n").expect("file");
        assert!(run(&["add", "README"]).status.success());
        assert!(run(&["commit", "-qm", "initial"]).status.success());
        for index in 1..=11 {
            assert!(
                run(&["commit", "--allow-empty", "-qm", &format!("commit-{index}"),])
                    .status
                    .success()
            );
        }

        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.load_history();
        assert!(app.status_message().contains("running"));
        for _ in 0..100 {
            app.poll_background();
            if !app.history.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.history[0].subject, "commit-11");
        assert_eq!(app.history.len(), 10);
        assert!(app.history_has_more());
        assert!(app.history_snapshot_head.is_some());
        app.output_scroll = 0;
        app.handle_output_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.commit_detail_active());
        for _ in 0..100 {
            app.poll_background();
            if app
                .commit_detail
                .as_ref()
                .is_some_and(|(_, _, text)| text.contains("commit-11"))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let patch_text = &app.commit_detail.as_ref().expect("commit detail").2;
        assert!(patch_text.contains("commit-11"));
        assert!(patch_text.contains("Date:"));
        assert!(!app.history_has_more() || app.history.len() == 10);
        app.handle_output_key(key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.diff_full_context());
        assert!(app.commit_detail_pending);
        for _ in 0..100 {
            app.poll_background();
            if !app.commit_detail_pending {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!app.commit_detail_pending);
        app.handle_output_key(key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app.diff_split());
        app.handle_output_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.commit_detail_active());
        assert_eq!(app.output_scroll(), 0);
        assert!(run(&[
            "commit",
            "--allow-empty",
            "-qm",
            "new-head-after-first-page"
        ])
        .status
        .success());
        app.focus = Focus::Output;
        app.handle_output_key(key(KeyCode::Char(']'), KeyModifiers::NONE));
        for _ in 0..100 {
            app.poll_background();
            if app.history.len() > 10 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.history.len(), 12);
        assert_eq!(app.history[0].subject, "commit-11");
        assert!(!app
            .history
            .iter()
            .any(|commit| commit.subject == "new-head-after-first-page"));
        assert!(!app.history_has_more());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn automation_mouse_coordinates_validate_click_and_drag() {
        assert_eq!(
            automation_mouse_coordinates("4 8", 2).expect("click coordinates"),
            [4, 8, 4, 8]
        );
        assert_eq!(
            automation_mouse_coordinates("1 2 3 4", 4).expect("drag coordinates"),
            [1, 2, 3, 4]
        );
        assert!(automation_mouse_coordinates("1", 2).is_err());
        assert!(automation_mouse_coordinates("a 2", 2).is_err());
    }

    #[test]
    fn automation_mouse_events_cover_click_right_click_and_hover() {
        let click = automation_mouse_events("click", "4 8").expect("click");
        assert!(matches!(
            click[0].kind,
            MouseEventKind::Down(MouseButton::Left)
        ));
        assert!(matches!(
            click[1].kind,
            MouseEventKind::Up(MouseButton::Left)
        ));
        let right = automation_mouse_events("rclick", "4 8").expect("right click");
        assert!(matches!(
            right[0].kind,
            MouseEventKind::Down(MouseButton::Right)
        ));
        let hover = automation_mouse_events("hover", "4 8").expect("hover");
        assert!(matches!(hover[0].kind, MouseEventKind::Moved));
        assert!(automation_mouse_events("unknown", "4 8").is_err());
    }

    #[test]
    fn automation_scroll_coordinates_validate_signed_bounded_count() {
        assert_eq!(
            automation_scroll_coordinates("4 8 -12").expect("scroll coordinates"),
            (4, 8, -12)
        );
        assert!(automation_scroll_coordinates("4 8 1001").is_err());
        assert!(automation_scroll_coordinates("4 8 nope").is_err());
    }

    #[test]
    fn secondary_cursor_inserts_are_one_undo_batch() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_key(key(KeyCode::Down, KeyModifiers::ALT))
            .expect("add cursor");
        assert_eq!(app.cursor_positions().len(), 2);
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("insert at cursors");
        assert_eq!(app.file_lines()[0], "x# mtide");
        assert_eq!(app.file_lines()[1], "x");
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo");
        assert_eq!(app.file_lines()[0], "# mtide");
        assert_eq!(app.file_lines()[1], "");
    }

    #[test]
    fn alt_shift_adds_repeated_cursors_above_and_below() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(
            vec!["zero".into(), "one".into(), "two".into(), "three".into()],
            None,
        );
        app.buffer_mut().set_cursor(1, 2);
        app.handle_key(key(KeyCode::Down, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("add cursor below");
        app.handle_key(key(KeyCode::Down, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("extend cursor stack below");
        assert_eq!(
            app.cursor_positions(),
            vec![
                Position { line: 1, column: 2 },
                Position { line: 2, column: 2 },
                Position { line: 3, column: 2 },
            ]
        );
        app.handle_key(key(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("add cursor above");
        assert_eq!(
            app.cursor_positions(),
            vec![
                Position { line: 1, column: 2 },
                Position { line: 0, column: 2 },
                Position { line: 2, column: 2 },
                Position { line: 3, column: 2 },
            ]
        );
    }

    #[test]
    fn alt_shift_keeps_moving_selected_lines() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["one".into(), "two".into()], None);
        app.selection_anchor = Some(Position { line: 1, column: 0 });
        app.buffer_mut().set_cursor(1, 3);
        app.handle_key(key(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("move selected line");
        assert_eq!(app.file_lines(), &["two", "one"]);
    }

    #[test]
    fn multi_cursor_paste_spreads_one_line_per_cursor() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["one".into(), "two".into()], None);
        app.handle_key(key(KeyCode::Down, KeyModifiers::ALT))
            .expect("add cursor");
        app.clipboard = "A\nB".to_owned();
        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .expect("paste");
        assert_eq!(app.file_lines(), &["Aone", "Btwo"]);
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo");
        assert_eq!(app.file_lines(), &["one", "two"]);
    }

    #[test]
    fn insert_and_shift_delete_clipboard_aliases_match_ctrl_shortcuts() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["one two".into()], None);
        app.selection_anchor = Some(Position { line: 0, column: 0 });
        app.buffer_mut().set_cursor(0, 3);
        app.handle_key(key(KeyCode::Insert, KeyModifiers::CONTROL))
            .expect("copy with ctrl insert");
        assert_eq!(app.clipboard, "one");

        app.selection_anchor = None;
        app.buffer_mut().move_end();
        app.handle_key(key(KeyCode::Insert, KeyModifiers::SHIFT))
            .expect("paste with shift insert");
        assert_eq!(app.file_lines(), &["one twoone"]);

        app.selection_anchor = Some(Position { line: 0, column: 0 });
        app.buffer_mut().set_cursor(0, 3);
        app.handle_key(key(KeyCode::Delete, KeyModifiers::SHIFT))
            .expect("cut with shift delete");
        assert_eq!(app.file_lines(), &[" twoone"]);
    }

    #[test]
    fn mouse_selection_updates_editor_cursor_and_selection() {
        let mut app = App::new(&[]).expect("welcome app");
        app.mouse_down(Position { line: 0, column: 1 }, false);
        app.mouse_drag(Position { line: 2, column: 2 });
        app.mouse_up();
        assert_eq!(app.cursor(), Position { line: 2, column: 2 });
        assert_eq!(app.selection_lines(), Some((0, 2)));
    }

    #[test]
    fn character_selection_is_rune_column_and_multiline_safe() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["你好abc".into(), "second".into()], None);
        app.mouse_down(Position { line: 0, column: 1 }, false);
        app.mouse_drag(Position { line: 1, column: 3 });
        assert!(app.is_position_selected(0, 1));
        assert!(app.is_position_selected(0, 4));
        assert!(app.is_position_selected(1, 0));
        assert!(app.is_position_selected(1, 2));
        assert!(!app.is_position_selected(1, 3));
    }

    #[test]
    fn column_selection_creates_independent_unicode_safe_ranges() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["abcd".into(), "12345".into(), "你好世界".into()], None);
        app.mouse_column_down(Position { line: 0, column: 1 });
        app.mouse_column_drag(Position { line: 2, column: 3 });
        app.mouse_up();
        assert_eq!(app.secondary_selections.len(), 2);
        app.replace_active_selections("X");
        assert_eq!(app.file_lines(), &["aXd", "1X45", "你X界"]);
        app.buffer_mut().undo();
        assert_eq!(app.file_lines(), &["abcd", "12345", "你好世界"]);
    }

    #[test]
    fn inline_conflict_resolution_replaces_only_the_selected_block() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(
            vec![
                "before".into(),
                "<<<<<<< ours".into(),
                "ours".into(),
                "=======".into(),
                "theirs".into(),
                ">>>>>>> theirs".into(),
                "after".into(),
            ],
            None,
        );
        app.handle_key(key(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("resolve ours");
        assert_eq!(app.file_lines(), &["before", "ours", "after"]);
        app.buffer_mut().undo();
        assert_eq!(app.file_lines()[1], "<<<<<<< ours");
    }

    #[test]
    fn conflict_preview_shows_three_way_sides_and_can_resolve() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(
            vec![
                "<<<<<<< ours".into(),
                "ours".into(),
                "||||||| base".into(),
                "base".into(),
                "=======".into(),
                "theirs".into(),
                ">>>>>>> theirs".into(),
            ],
            None,
        );
        app.handle_key(key(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("preview");
        match app.lsp_popup() {
            Some(LspPopup::ConflictPreview { lines }) => {
                assert!(lines.iter().any(|line| line == "Ours"));
                assert!(lines.iter().any(|line| line == "Base"));
                assert!(lines.iter().any(|line| line == "Theirs"));
            }
            _ => panic!("expected conflict preview"),
        }
        app.handle_key(key(KeyCode::Char('b'), KeyModifiers::NONE))
            .expect("choose base");
        assert_eq!(app.file_lines(), &["base"]);
    }

    #[test]
    fn inline_conflict_resolution_can_choose_base() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(
            vec![
                "<<<<<<< ours".into(),
                "ours".into(),
                "||||||| base".into(),
                "base".into(),
                "=======".into(),
                "theirs".into(),
                ">>>>>>> theirs".into(),
            ],
            None,
        );
        app.handle_key(key(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("resolve base");
        assert_eq!(app.file_lines(), &["base"]);
    }

    #[test]
    fn inline_conflict_resolution_can_keep_both_sides() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(
            vec![
                "<<<<<<< ours".into(),
                "ours".into(),
                "=======".into(),
                "theirs".into(),
                ">>>>>>> theirs".into(),
            ],
            None,
        );
        app.handle_key(key(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("resolve both");
        assert_eq!(app.file_lines(), &["ours", "theirs"]);
        app.buffer_mut().undo();
        assert_eq!(app.file_lines()[0], "<<<<<<< ours");
    }

    #[test]
    fn conflict_navigation_cycles_through_inline_blocks() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(
            vec![
                "<<<<<<< ours".into(),
                "one".into(),
                "=======".into(),
                "two".into(),
                ">>>>>>> theirs".into(),
                "middle".into(),
                "<<<<<<< ours".into(),
                "three".into(),
                "=======".into(),
                "four".into(),
                ">>>>>>> theirs".into(),
            ],
            None,
        );
        app.handle_key(key(
            KeyCode::Down,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("next conflict");
        assert_eq!(app.cursor().line, 6);
        app.handle_key(key(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::ALT))
            .expect("previous conflict");
        assert_eq!(app.cursor().line, 0);
    }

    #[test]
    fn alt_mouse_click_adds_and_removes_secondary_cursor() {
        let mut app = App::new(&[]).expect("welcome app");
        app.mouse_add_cursor(Position { line: 1, column: 0 });
        assert_eq!(app.cursor_positions().len(), 2);
        app.mouse_add_cursor(Position { line: 1, column: 0 });
        assert_eq!(app.cursor_positions().len(), 1);
    }

    #[test]
    fn select_next_replaces_all_selected_occurrences_in_one_undo() {
        let path = std::env::temp_dir().join(format!("mtide-select-next-{}", std::process::id()));
        std::fs::write(&path, "one one one").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.selection_anchor = Some(Position { line: 0, column: 0 });
        app.buffer_mut().set_cursor(0, 3);
        app.handle_key(key(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("select next");
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("replace selections");
        assert_eq!(app.file_lines(), &["x x one"]);
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo");
        assert_eq!(app.file_lines(), &["one one one"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn terminal_focus_can_create_and_switch_pty_tabs() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Terminal;
        app.handle_key(key(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("create terminal");
        assert_eq!(app.terminal_count(), 2);
        assert_eq!(app.active_terminal(), 1);
        app.handle_key(key(
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("switch terminal");
        assert_eq!(app.active_terminal(), 0);
    }

    #[test]
    fn terminal_alt_t_toggles_reversible_fullscreen_layout() {
        let mut app = App::new(&[]).expect("welcome app");
        let sidebar = app.sidebar_visible;
        let agent = app.agent_visible;
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::ALT))
            .expect("enter fullscreen");
        assert!(app.terminal_fullscreen());
        assert!(app.bottom_visible());
        assert_eq!(app.focus(), Focus::Terminal);
        assert!(!app.sidebar_visible);
        assert!(!app.agent_visible);
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::ALT))
            .expect("leave fullscreen");
        assert!(!app.terminal_fullscreen());
        assert_eq!(app.sidebar_visible, sidebar);
        assert_eq!(app.agent_visible, agent);
        assert_eq!(app.focus(), Focus::Editor);
    }

    #[test]
    fn editor_page_navigation_moves_by_a_viewport_and_preserves_shift_selection() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new((0..30).map(|line| format!("line {line}")).collect(), None);
        app.handle_key(key(KeyCode::PageDown, KeyModifiers::NONE))
            .expect("page down");
        assert_eq!(app.cursor().line, 10);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::SHIFT))
            .expect("select page up");
        assert_eq!(app.cursor().line, 0);
        assert_eq!(app.selection_lines(), Some((0, 10)));
    }

    #[test]
    fn ctrl_enter_inserts_an_indented_line_below_and_is_undoable() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["  value".into(), "next".into()], None);
        app.buffer_mut().set_cursor(0, 3);
        app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL))
            .expect("insert line below");
        assert_eq!(app.file_lines(), &["  value", "  ", "next"]);
        assert_eq!(app.cursor(), Position { line: 1, column: 2 });
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo inserted line");
        assert_eq!(app.file_lines(), &["  value", "next"]);
    }

    #[test]
    fn matching_bracket_navigation_handles_nested_multiline_pairs() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["fn() {".into(), "  [value]".into(), "}".into()], None);
        app.buffer_mut().set_cursor(0, 5);
        app.handle_key(key(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("jump forward");
        assert_eq!(app.cursor(), Position { line: 2, column: 0 });
        app.handle_key(key(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("jump backward");
        assert_eq!(app.cursor(), Position { line: 0, column: 5 });
    }

    #[test]
    fn agent_install_completion_restarts_the_selected_provider_session() {
        let mut app = App::new(&[]).expect("welcome app");
        let generation = app.agent_tool_generation.load(Ordering::Acquire);
        app.settings.agent.providers.insert(
            "test".to_owned(),
            crate::config::AgentProviderConfig {
                extra: std::collections::BTreeMap::new(),
                name: "Test".to_owned(),
                command: vec!["sh".to_owned()],
                cwd: None,
                install: Some("echo install".to_owned()),
                tools: Vec::new(),
                persist: false,
            },
        );
        app.agent_provider = "test".to_owned();
        app.agent_install_pending = true;
        app.poll_background();
        assert!(!app.agent_install_pending);
        assert!(app.status_message().contains("session restarted"));
        assert!(app.agent_tool_generation.load(Ordering::Acquire) > generation);
        assert!(app.agent_protocol_buffer.is_empty());
    }

    #[test]
    fn agent_protocol_replies_when_tool_call_omits_tool_name() {
        let mut app = App::new(&[]).expect("welcome app");
        app.agent_protocol_buffer = br#"{"type":"tool_call","id":"missing-tool"}
"#
        .to_vec();

        app.poll_agent_protocol();

        let completion = app.agent_tool_rx.try_recv().expect("tool error response");
        assert_eq!(completion.response["type"], "tool_result");
        assert_eq!(completion.response["id"], "missing-tool");
        assert_eq!(completion.response["ok"], false);
        assert!(completion.response["error"]
            .as_str()
            .expect("error")
            .contains("requires a string tool field"));
    }

    #[test]
    fn terminal_links_reject_non_http_protocols() {
        let mut app = App::new(&[]).expect("welcome app");
        app.open_terminal_link("file:///etc/passwd");
        assert!(app.status_message().contains("valid HTTP(S)"));
    }

    #[test]
    fn http_link_validation_requires_a_host_and_rejects_control_characters() {
        assert!(valid_http_url("https://example.test/path?q=1"));
        assert!(valid_http_url("http://[::1]:8080/path"));
        assert!(!valid_http_url("https:///missing-host"));
        assert!(!valid_http_url("file:///etc/passwd"));
        assert!(!valid_http_url("https://example.test\nxdg-open"));
        assert!(!valid_http_url("https://:8080/path"));
    }

    #[test]
    fn editor_link_lookup_handles_unicode_columns_and_punctuation() {
        let line = "说明 https://example.com/path?q=1，继续";
        let column = line.chars().position(|character| character == 'e').unwrap();
        assert_eq!(
            editor_link_at(line, column).as_deref(),
            Some("https://example.com/path?q=1")
        );
        assert_eq!(editor_link_at(line, 0), None);
        assert_eq!(
            editor_link_at("http://a.test", 12),
            Some("http://a.test".into())
        );
        assert_eq!(
            editor_link_at("src/main.rs:12:7", 4),
            Some("src/main.rs:12:7".into())
        );
        assert_eq!(editor_link_at("word", 2), None);
        assert_eq!(
            parse_editor_file_reference(r"C:\work\main.rs:12:7"),
            Some(TargetSpec {
                path: PathBuf::from(r"C:\work\main.rs"),
                line: 11,
                column: 6,
            })
        );
    }

    #[test]
    fn editor_file_references_resolve_from_source_directory_and_navigate_to_position() {
        let root = std::env::temp_dir().join(format!("mtide-link-{}", std::process::id()));
        let source_dir = root.join("src");
        std::fs::create_dir_all(&source_dir).expect("source directory");
        let source = source_dir.join("notes.md");
        let target = root.join("target.rs");
        let text = "See ../target.rs:2:3.";
        std::fs::write(&source, text).expect("source file");
        std::fs::write(&target, "first\nsecond\n").expect("target file");
        let mut app = App::new(&[source.to_string_lossy().into_owned()]).expect("app");
        let column = text
            .chars()
            .position(|character| character == 't')
            .expect("target column");
        let position = Position { line: 0, column };

        assert_eq!(
            app.editor_link_at(position).as_deref(),
            Some("../target.rs:2:3")
        );
        assert!(app.open_editor_link(position));
        assert_eq!(app.buffer().path(), Some(target.as_path()));
        assert_eq!(app.cursor(), Position { line: 1, column: 2 });
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn editor_link_feedback_clears_on_input_and_focus_loss() {
        let path = std::env::temp_dir().join(format!("mtide-link-hover-{}.md", std::process::id()));
        std::fs::write(&path, "https://example.test").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        let position = Position { line: 0, column: 8 };

        app.update_editor_hover(Some(position), true);
        assert_eq!(app.editor_hover_link(0), Some("https://example.test"));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .expect("input clears link feedback");
        assert_eq!(app.editor_hover_link(0), None);

        app.update_editor_hover(Some(position), true);
        app.handle_focus_event(false).expect("focus event");
        assert_eq!(app.editor_hover_link(0), None);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn missing_file_link_can_be_hovered_without_resolving_filesystem_path() {
        let source =
            std::env::temp_dir().join(format!("mtide-link-candidate-{}.md", std::process::id()));
        std::fs::write(&source, "See missing.rs:2").expect("source file");
        let mut app = App::new(&[source.to_string_lossy().into_owned()]).expect("app");
        let position = Position { line: 0, column: 8 };
        app.update_editor_hover(Some(position), true);
        assert_eq!(app.editor_hover_link(0), Some("missing.rs:2"));
        assert!(!app.open_editor_link(position));
        std::fs::remove_file(source).expect("cleanup");
    }

    #[test]
    fn terminal_link_lookup_works_while_scrolled_into_history() {
        let mut app = App::new(&[]).expect("welcome app");
        app.terminals[app.active_terminal]
            .feed_output(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07\n");
        app.terminals[app.active_terminal].scroll(1);
        assert_eq!(
            app.terminal_link_at(0, 0).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn ctrl_f_inherits_single_line_selection_into_search() {
        let path =
            std::env::temp_dir().join(format!("mtide-find-selection-{}", std::process::id()));
        std::fs::write(&path, "needle here").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.selection_anchor = Some(Position { line: 0, column: 0 });
        app.buffer_mut().set_cursor(0, 6);
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .expect("open search");
        assert_eq!(app.focus(), Focus::Search);
        assert_eq!(app.search_query(), "needle");
        assert!(!app.search_results().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ctrl_f_inherits_column_and_secondary_selections_into_search() {
        let root =
            std::env::temp_dir().join(format!("mtide-find-multiselection-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let path = root.join("main.txt");
        std::fs::write(&path, "alpha beta\nalpha beta\n").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.column_selection = Some((
            Position { line: 0, column: 0 },
            Position { line: 0, column: 5 },
        ));
        app.buffer_mut().set_cursor(0, 5);
        app.secondary_selections.push((
            Position { line: 1, column: 0 },
            Position { line: 1, column: 5 },
        ));
        app.selection_anchor = None;
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .expect("open search");
        assert_eq!(app.search_query(), "alpha");
        assert_eq!(app.focus(), Focus::Search);
        assert!(!app.search_results().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_ctrl_alt_r_toggles_regex_mode() {
        let root = std::env::temp_dir().join(format!("mtide-search-regex-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let path = root.join("main.rs");
        std::fs::write(&path, "needle\nneedles\n").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.focus = Focus::Search;
        app.search_query = "needle.".to_owned();
        app.refresh_search();
        assert!(!app.search_regex);
        app.handle_key(key(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("toggle regex");
        assert!(app.search_regex);
        assert_eq!(app.search_results().len(), 1);
        app.handle_key(key(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("toggle smart case");
        assert!(!app.search_regex);
        assert!(app.search_results().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn super_modifier_activates_existing_control_shortcuts() {
        let mut app = App::new(&[]).expect("app");
        app.settings.keyboard.modifier_key = "control+super".to_owned();
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::SUPER))
            .expect("super find");
        assert_eq!(app.focus(), Focus::Search);
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close search");
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::SUPER))
            .expect("super terminal toggle");
        assert!(app.bottom_visible());
    }

    #[test]
    fn super_modifier_is_preserved_for_plugin_events_and_automation() {
        assert_eq!(
            format_key_modifier(KeyModifiers::SUPER | KeyModifiers::SHIFT),
            "shift+super"
        );
        let events = automation_key_events("super+f").expect("super automation key");
        assert_eq!(events, vec![key(KeyCode::Char('f'), KeyModifiers::SUPER)]);
    }

    #[test]
    fn search_panel_cycles_query_include_and_exclude_fields() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Search;
        app.search_exclude.clear();
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("query input");
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("include field");
        app.handle_paste("*.rs").expect("include input");
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("exclude field");
        app.handle_paste("target").expect("exclude input");
        assert_eq!(app.search_query(), "n");
        assert_eq!(app.search_include(), "*.rs");
        assert_eq!(app.search_exclude(), "target");
        assert_eq!(app.search_field(), 2);
        app.handle_key(key(KeyCode::BackTab, KeyModifiers::SHIFT))
            .expect("previous search field");
        assert_eq!(app.search_field(), 1);
    }

    #[test]
    fn replace_mode_can_refocus_from_editor_without_reopening_search() {
        let mut app = App::new(&[]).expect("welcome app");
        app.sidebar_tab = SidebarTab::Search;
        app.replace_mode = true;
        app.search_query = "needle".into();
        app.replace_query = "done".into();
        app.focus = Focus::Editor;
        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .expect("refocus replace bar");
        assert_eq!(app.focus(), Focus::Search);
        assert!(app.replace_mode());
        assert_eq!(app.search_query(), "needle");
        assert_eq!(app.replace_query(), "done");
    }

    #[test]
    fn ctrl_r_from_editor_opens_and_focuses_replace_field() {
        let mut app = App::new(&[]).expect("welcome app");
        app.sidebar_tab = SidebarTab::Search;
        app.search_query = "needle".into();
        app.search_field = 2;
        app.focus = Focus::Editor;
        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .expect("open replace mode");
        assert_eq!(app.focus(), Focus::Search);
        assert!(app.replace_mode());
        assert_eq!(app.search_field(), 0);
        assert_eq!(app.search_query(), "needle");
    }

    #[test]
    fn escape_from_editor_closes_replace_mode() {
        let mut app = App::new(&[]).expect("welcome app");
        app.sidebar_tab = SidebarTab::Search;
        app.replace_mode = true;
        app.focus = Focus::Editor;
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close replace bar");
        assert_eq!(app.focus(), Focus::Editor);
        assert!(!app.replace_mode());
    }

    #[test]
    fn replace_one_refreshes_stale_search_results_before_editing() {
        let root = std::env::temp_dir().join(format!("mtide-replace-one-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("workspace");
        let path = root.join("main.rs");
        std::fs::write(&path, "needle\nneedle").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.search_query = "needle".to_owned();
        app.refresh_search_results();
        assert_eq!(app.search_results.len(), 2);
        app.buffer_mut()
            .replace_line_range(0, 1, &["changed".to_owned()]);
        app.replace_mode = true;
        app.replace_query = "done".to_owned();
        app.focus = Focus::Search;
        app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL))
            .expect("replace one");
        assert_eq!(app.file_lines(), &["changed", "done"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn editor_mouse_scroll_viewport_is_clamped_and_shift_scrolls_horizontally() {
        let mut app = App::new(&[]).expect("app");
        app.buffers[0] = Buffer::new((0..20).map(|index| format!("line {index}")).collect(), None);
        app.scroll_editor(7);
        assert_eq!(app.editor_scroll_line(), Some(7));
        app.scroll_editor(-20);
        assert_eq!(app.editor_scroll_line(), Some(0));
        app.set_editor_viewport_width(0);
        app.scroll_editor_horizontal(8);
        assert_eq!(app.horizontal_scroll, 8);
        app.scroll_editor_horizontal(-20);
        assert_eq!(app.horizontal_scroll, 0);
    }

    #[test]
    fn search_results_follow_unsaved_active_buffer_edits() {
        let root = std::env::temp_dir().join(format!("mtide-live-search-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let path = root.join("main.rs");
        std::fs::write(&path, "needle").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.search_query = "needle".to_owned();
        app.refresh_search();
        assert_eq!(app.search_results().len(), 1);
        app.buffer_mut().move_end();
        app.handle_paste(" needle").expect("edit active buffer");
        assert_eq!(app.search_results().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_results_follow_unsaved_buffer_when_switching_tabs() {
        let root =
            std::env::temp_dir().join(format!("mtide-live-search-tabs-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::write(&first, "needle").expect("first file");
        std::fs::write(&second, "plain").expect("second file");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.search_query = "needle".to_owned();
        app.refresh_search();
        assert_eq!(app.search_results().len(), 1);
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.handle_paste(" needle").expect("edit second buffer");
        assert_eq!(app.search_results().len(), 2);
        app.switch_tab(-1);
        assert_eq!(app.search_results().len(), 2);
        app.switch_tab(1);
        assert_eq!(app.search_results().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_state_refreshes_when_completion_targets_a_previous_root() {
        let root =
            std::env::temp_dir().join(format!("mtide-git-root-switch-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.poll_background();
        let generation = app.git_state_generation.load(Ordering::Acquire);
        app.git_state_pending = true;
        app.git_state_tx
            .send(GitStateCompletion {
                generation,
                active_root: root.join("previous-root"),
                changes: Vec::new(),
                change_roots: Vec::new(),
                changes_by_root: BTreeMap::new(),
                branch: None,
                branches: Vec::new(),
                change_diff: Vec::new(),
            })
            .expect("completion channel");

        app.poll_background();

        assert!(app.git_state_pending);
        assert!(app.git_state_generation.load(Ordering::Acquire) > generation);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn interactive_search_discards_stale_generations() {
        let root = std::env::temp_dir().join(format!(
            "mtide-search-generation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(root.join("main.rs"), "old needle\nnew needle\n").expect("source");
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.search_query = "old".to_owned();
        app.request_search_refresh();
        app.search_query = "new".to_owned();
        app.request_search_refresh();

        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            app.poll_background();
            if !app.search_pending {
                break;
            }
        }
        assert!(!app.search_pending);
        assert_eq!(app.search_results.len(), 1);
        assert_eq!(app.search_results[0].text, "new needle");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn closing_active_tab_notifies_plugin_of_new_active_tab() {
        let root = std::env::temp_dir().join(format!("mtide-tab-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let plugin_root = root.join("plugin");
        std::fs::create_dir_all(&workspace).expect("workspace directory");
        std::fs::create_dir_all(&plugin_root).expect("plugin directory");
        let first = workspace.join("first.rs");
        let second = workspace.join("second.rs");
        std::fs::write(&first, "first").expect("first");
        std::fs::write(&second, "second").expect("second");
        std::fs::write(
            plugin_root.join("plugin.ttt.json"),
            r#"{"name":"tab-events","entry":"init.lua","api":2,"permissions":{"events.editor":true,"events.file":true,"panel.editor":true}}"#,
        )
        .expect("manifest");
        std::fs::write(
            plugin_root.join("init.lua"),
            r#"local ttt = require("ttt")
local events = require("ttt.events")
events.on("tab.change", function(path) ttt.log("active:" .. path) end)
events.on("tab.close", function(path) ttt.log("close:" .. path) end)
events.on("file.close", function(path) ttt.log("file-close:" .. path) end)
ttt.open_tab({id = "custom", title = "Custom", render = function(panel) panel:label("custom") end})
"#,
        )
        .expect("entry");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.plugin_runtimes.push(
            PluginRuntime::load(&PluginPackage::load(&plugin_root).expect("package"))
                .expect("runtime"),
        );
        let plugin_index = app.plugin_runtimes.len() - 1;
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.close_active_tab();
        let close = format!("[info] close:{}", second.display());
        let file_close = format!("[info] file-close:{}", second.display());
        let active = format!("[info] active:{}", first.display());
        assert!(app.plugin_runtimes.iter().any(|runtime| {
            let logs = runtime.logs();
            let close_index = logs.iter().position(|line| line == &close);
            let file_close_index = logs.iter().position(|line| line == &file_close);
            let active_index = logs.iter().position(|line| line == &active);
            file_close_index.is_some_and(|file| {
                close_index.is_some_and(|index| {
                    active_index.is_some_and(|active| file < index && index < active)
                })
            })
        }));
        app.plugin_custom_tab = Some((plugin_index, "custom".to_owned()));
        app.plugin_file_view = Some(("Custom".to_owned(), "custom".to_owned()));
        app.close_plugin_file_view();
        let logs = app.plugin_runtimes[plugin_index].logs();
        let close = format!("[info] close:{}", first.display());
        let active = format!("[info] active:{}", first.display());
        let close_index = logs
            .iter()
            .rposition(|line| line == &close)
            .expect("custom close");
        assert!(logs
            .iter()
            .enumerate()
            .skip(close_index + 1)
            .any(|(_, line)| line == &active));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn custom_plugin_tab_routes_selection_navigation_and_activation() {
        let root =
            std::env::temp_dir().join(format!("mtide-custom-tab-selection-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("plugin directory");
        std::fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"custom-tab-selection","entry":"init.lua","api":2,"permissions":{"panel.editor":true}}"#,
        )
        .expect("manifest");
        std::fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.open_tab({id = "items", title = "Items", render = function(panel)
  panel:list({items = {{label = "one"}, {label = "two"}}, on_select = function(item)
    ttt.log("selected:" .. item.label)
  end})
end})
"#,
        )
        .expect("entry");
        let mut app = App::new(&[]).expect("app");
        app.plugin_runtimes.push(
            PluginRuntime::load(&PluginPackage::load(&root).expect("package")).expect("runtime"),
        );
        let runtime_index = app.plugin_runtimes.len() - 1;
        app.plugin_custom_tab = Some((runtime_index, "items".to_owned()));
        app.plugin_file_view = Some(("Items".to_owned(), String::new()));
        app.refresh_plugin_custom_tab();
        app.handle_plugin_file_key(key(KeyCode::Down, KeyModifiers::NONE));
        app.handle_plugin_file_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.plugin_runtimes[runtime_index]
            .logs()
            .iter()
            .any(|line| line == "[info] selected:two"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mouse_cursor_and_selection_changes_emit_plugin_cursor_events() {
        let root = std::env::temp_dir().join(format!(
            "mtide-mouse-events-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let workspace = root.join("workspace");
        let plugin_root = root.join("plugin");
        std::fs::create_dir_all(&workspace).expect("workspace directory");
        std::fs::create_dir_all(&plugin_root).expect("plugin directory");
        let file = workspace.join("main.rs");
        std::fs::write(&file, "first\nsecond\n").expect("source");
        std::fs::write(
            plugin_root.join("plugin.ttt.json"),
            r#"{"name":"mouse-events","entry":"init.lua","api":2,"permissions":{"events.editor":true}}"#,
        )
        .expect("manifest");
        std::fs::write(
            plugin_root.join("init.lua"),
            r#"local ttt = require("ttt")
local events = require("ttt.events")
events.on("cursor.change", function(path, line, col, language, active, text, current, count, dirty, sl, sc, el, ec, word)
  ttt.log(path .. ":" .. line .. ":" .. col .. ":" .. sl .. ":" .. ec)
end)
"#,
        )
        .expect("entry");
        let mut app = App::new(&[file.to_string_lossy().into_owned()]).expect("app");
        app.plugin_runtimes.push(
            PluginRuntime::load(&PluginPackage::load(&plugin_root).expect("package"))
                .expect("runtime"),
        );
        let plugin_index = app.plugin_runtimes.len() - 1;

        app.handle_editor_key(key(KeyCode::Right, KeyModifiers::SHIFT))
            .expect("shift selection");
        app.clipboard = "X".to_owned();
        app.handle_editor_key(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .expect("paste selection");
        app.focus = Focus::Editor;
        app.handle_paste("Y").expect("system paste");

        app.mouse_down(Position { line: 0, column: 1 }, false);
        app.mouse_drag(Position { line: 1, column: 3 });
        app.mouse_up();

        let logs = app.plugin_runtimes[plugin_index].logs();
        assert!(
            logs.len() >= 3,
            "expected keyboard and mouse cursor events: {logs:?}"
        );
        assert!(logs.iter().any(|line| line.contains(":1:2:1:2")));
        assert!(logs.iter().any(|line| line.contains(":2:4:1:4")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_shift_t_reopens_the_most_recent_clean_tab() {
        let root = std::env::temp_dir().join(format!(
            "mtide-reopen-tab-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).expect("workspace");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::write(&first, "first").expect("first");
        std::fs::write(&second, "second").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("close tab");
        assert_eq!(app.buffers.len(), 1);
        assert_eq!(app.closed_tabs.len(), 1);
        assert_eq!(app.focus(), Focus::Editor);
        app.handle_key(key(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("reopen tab");
        assert_eq!(app.buffers.len(), 2);
        assert!(app.file_name().ends_with("second.rs"));
        assert!(app.closed_tabs.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_enter_navigates_existing_unsaved_tab_in_memory() {
        let root =
            std::env::temp_dir().join(format!("mtide-search-open-tab-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::write(&first, "needle").expect("first file");
        std::fs::write(&second, "plain").expect("second file");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.handle_paste("x").expect("dirty first buffer");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.search_query = "needle".to_owned();
        app.refresh_search();
        let index = app
            .search_results
            .iter()
            .position(|result| result.path == first)
            .expect("first result");
        app.search_index = index;
        app.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.buffer().path(), Some(first.as_path()));
        assert_eq!(app.cursor().column, 1);
        assert_eq!(app.file_lines(), &vec!["xneedle".to_owned()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bracketed_paste_routes_to_editor_and_search_inputs() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_paste("你好\nworld").expect("editor paste");
        assert_eq!(app.file_lines()[0], "你好");
        app.focus = Focus::Search;
        app.handle_paste("needle").expect("search paste");
        assert_eq!(app.search_query(), "needle");
    }

    #[test]
    fn find_replace_paste_and_unicode_cursor_edit_only_focused_field() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Search;
        app.search_query = "猫狗".into();
        app.search_cursor = 1;
        app.handle_paste("🐈").expect("paste into search");
        assert_eq!(app.search_query(), "猫🐈狗");
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
            .expect("delete before search cursor");
        assert_eq!(app.search_query(), "猫狗");

        app.replace_mode = true;
        app.replace_cursor = 0;
        app.handle_paste("完成").expect("paste into replace field");
        assert_eq!(app.replace_query(), "完成");
        assert_eq!(app.search_query(), "猫狗");

        app.search_field = 1;
        app.search_cursor = 0;
        app.clipboard = "*.rs".to_owned();
        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::CONTROL))
            .expect("Ctrl-V into include field");
        assert_eq!(app.search_include(), "*.rs");
        assert_eq!(app.search_query(), "猫狗");
        assert_eq!(app.replace_query(), "完成");
    }

    #[test]
    fn search_mouse_columns_map_wide_unicode_to_character_cursor() {
        let mut app = App::new(&[]).expect("welcome app");
        app.search_query = "A猫B".into();
        app.set_search_field(0, 3);
        assert_eq!(app.search_input_cursor(), 2);
        app.set_search_field(0, 2);
        assert_eq!(app.search_input_cursor(), 2);
    }

    #[test]
    fn quick_open_and_goto_inputs_support_cursor_editing_and_paste() {
        let mut app = App::new(&[]).expect("welcome app");
        app.quick_open = true;
        app.quick_open_query = "abc".into();
        app.quick_open_cursor = 3;
        app.handle_quick_open_key(key(KeyCode::Home, KeyModifiers::NONE))
            .expect("quick open home");
        app.handle_paste("中").expect("quick open paste");
        assert_eq!(app.quick_open_query(), "中abc");
        app.goto_mode = true;
        app.quick_open = false;
        app.goto_input = "12:3".into();
        app.goto_cursor = app.goto_input.chars().count();
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .expect("goto left");
        app.handle_key(key(KeyCode::Char('4'), KeyModifiers::NONE))
            .expect("goto insert");
        assert_eq!(app.goto_input(), "12:43");
    }

    #[test]
    fn paste_routes_to_plugin_command_line_overlay() {
        let mut app = App::new(&[]).expect("welcome app");
        app.plugin_command_line = Some(PluginCommandLine {
            runtime_index: 0,
            id: 1,
            prefix: "/".into(),
            text: "initial".into(),
            cursor: "initial".chars().count(),
        });
        app.handle_paste(" pasted\ntext")
            .expect("command line paste");
        assert_eq!(
            app.plugin_command_line().expect("command line").text,
            "initial pasted\ntext"
        );
        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE))
            .expect("command line home");
        app.handle_key(key(KeyCode::Char('!'), KeyModifiers::NONE))
            .expect("command line insert");
        assert_eq!(
            app.plugin_command_line().expect("command line").text,
            "!initial pasted\ntext"
        );
        assert_eq!(app.file_lines()[0], "# mtide");
    }

    #[test]
    fn paste_routes_to_plugin_panel_input_and_password() {
        let root =
            std::env::temp_dir().join(format!("mtide-plugin-paste-input-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("plugin directory");
        std::fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"paste-input","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        std::fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "paste", render = function(panel)
  panel:input({text = "", autofocus = true})
  panel:password({text = ""})
end}})"#,
        )
        .expect("entry");

        let mut app = App::new(&[]).expect("app");
        let plugin_index = app.plugin_runtimes.len();
        app.plugin_runtimes.push(
            PluginRuntime::load(&PluginPackage::load(&root).expect("package")).expect("runtime"),
        );
        app.focus = Focus::Explorer;
        let initial_render = app.plugin_runtimes[plugin_index]
            .render_panel("paste")
            .expect("render");
        assert!(
            initial_render.is_some(),
            "logs={:?} panels={:?}",
            app.plugin_runtimes[plugin_index].logs(),
            app.plugin_runtimes[plugin_index].registration().panels
        );
        assert!(app.plugin_runtimes[plugin_index].focus_panel_input("paste", 0));
        app.handle_paste("hello").expect("input paste");
        let rendered = app.plugin_runtimes[plugin_index]
            .render_panel("paste")
            .expect("rerender")
            .expect("panel text");
        assert!(rendered.contains("hello"));

        assert!(app.plugin_runtimes[plugin_index].focus_panel_password("paste", 1));
        app.handle_paste("secret").expect("password paste");
        let rendered = app.plugin_runtimes[plugin_index]
            .render_panel("paste")
            .expect("password rerender")
            .expect("panel text");
        assert!(rendered.contains("••••••"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn quick_open_discovers_workspace_files_with_fuzzy_filtering() {
        let root = std::env::temp_dir().join(format!("mtide-quick-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::create_dir_all(root.join(".hidden")).expect("hidden");
        std::fs::create_dir_all(root.join("target")).expect("target");
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("gitignore");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("main");
        std::fs::write(root.join(".hidden/secret.rs"), "secret").expect("secret");
        std::fs::write(root.join("target/generated.rs"), "generated").expect("generated");
        std::fs::write(root.join("ignored.rs"), "ignored").expect("ignored");
        let _ = std::process::Command::new("git")
            .args(["-C", &root.to_string_lossy(), "init", "-q"])
            .status();
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.open_quick_open();
        assert!(app
            .quick_open_results()
            .iter()
            .any(|path| path.ends_with("src/main.rs")));
        assert!(!app
            .quick_open_results()
            .iter()
            .any(|path| path.to_string_lossy().contains(".hidden")));
        assert!(!app
            .quick_open_results()
            .iter()
            .any(|path| path.to_string_lossy().contains("target")));
        assert!(!app
            .quick_open_results()
            .iter()
            .any(|path| path.ends_with("ignored.rs")));
        assert!(!app
            .explorer_paths()
            .iter()
            .any(|path| path.to_string_lossy().contains(".hidden")));
        assert!(!app
            .explorer_paths()
            .iter()
            .any(|path| path.ends_with("ignored.rs")));
        app.settings.quick_open.show_hidden = true;
        app.settings.quick_open.show_git_ignored = true;
        app.refresh_quick_open_results();
        assert!(app
            .quick_open_results()
            .iter()
            .any(|path| path.ends_with(".hidden/secret.rs")));
        assert!(app
            .quick_open_results()
            .iter()
            .any(|path| path.ends_with("ignored.rs")));
        app.settings.explorer.show_hidden = true;
        app.settings.explorer.show_git_ignored = true;
        app.refresh_explorer();
        assert!(app
            .explorer_paths()
            .iter()
            .any(|path| path.to_string_lossy().contains(".hidden")));
        assert!(app
            .explorer_paths()
            .iter()
            .any(|path| path.ends_with("ignored.rs")));
        for character in "main".chars() {
            app.handle_quick_open_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("filter");
        }
        assert_eq!(app.quick_open_results().len(), 1);
        app.handle_quick_open_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open");
        assert_eq!(
            app.buffer().path(),
            Some(root.join("src/main.rs").as_path())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn quick_open_gt_switches_to_command_mode() {
        let mut app = App::new(&[]).expect("welcome app");
        assert!(app.sidebar_visible());
        app.open_quick_open();
        app.handle_quick_open_key(key(KeyCode::Char('>'), KeyModifiers::NONE))
            .expect("command mode");
        for character in "view.toggleSidebar".chars() {
            app.handle_quick_open_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("command query");
        }
        assert_eq!(app.quick_open_title(), "Command Palette");
        assert_eq!(
            app.command_palette_items_for_quick_open(),
            vec![("view.toggleSidebar".into(), "Toggle Sidebar".into())]
        );
        app.handle_quick_open_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("execute command");
        assert!(!app.sidebar_visible());
        assert!(!app.quick_open_open());
    }

    #[test]
    fn quick_open_question_mark_browses_keyboard_help() {
        let mut app = App::new(&[]).expect("welcome app");
        app.open_quick_open();
        app.handle_quick_open_key(key(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("help mode");
        for character in "terminal".chars() {
            app.handle_quick_open_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("help query");
        }
        assert_eq!(app.quick_open_title(), "Keyboard Help");
        assert_eq!(
            app.quick_open_help_items(),
            vec![
                ("Ctrl-K T".into(), "Open a new terminal".into()),
                ("Ctrl-T".into(), "Toggle terminal".into()),
                ("Alt-T".into(), "Toggle fullscreen terminal".into()),
            ]
        );
        app.handle_quick_open_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("close help");
        assert!(!app.quick_open_open());
    }

    #[test]
    fn ctrl_k_p_opens_quick_open_and_other_chords_focus_sidebar() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start chord");
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::NONE))
            .expect("open quick open");
        assert!(app.quick_open_open());
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close quick open");
        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start explorer chord");
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::NONE))
            .expect("focus explorer");
        assert_eq!(app.focus(), Focus::Explorer);
    }

    #[test]
    fn ctrl_k_t_opens_a_new_terminal_tab() {
        let mut app = App::new(&[]).expect("welcome app");
        let initial = app.terminals.len();
        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .expect("start terminal chord");
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("open terminal");
        assert_eq!(app.terminals.len(), initial + 1);
        assert_eq!(app.active_terminal, initial);
        assert_eq!(app.focus(), Focus::Terminal);
        assert_eq!(app.bottom_tab, BottomTab::Terminal);
    }

    #[test]
    fn terminal_selection_copies_visible_rows() {
        let mut app = App::new(&[]).expect("welcome app");
        let output = (0..24)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        app.terminals[0].feed_output(output.as_bytes());
        let lines = app.terminals[0].screen_lines();
        let base = lines.len().saturating_sub(3);
        let first = lines
            .iter()
            .position(|line| line.contains("line21"))
            .expect("first line");
        let second = lines
            .iter()
            .position(|line| line.contains("line22"))
            .expect("second line");
        app.begin_terminal_selection(0, (first - base) as u16);
        app.update_terminal_selection(80, (second - base) as u16);
        app.finish_terminal_selection();
        let selected = app.terminal_selection_text().expect("selection");
        assert!(selected.contains("line21"));
        assert!(selected.contains("line22"));
        app.copy_terminal_selection();
        assert_eq!(app.clipboard, selected);
    }

    #[test]
    fn terminal_selection_keeps_combining_marks_in_their_cell() {
        let mut app = App::new(&[]).expect("welcome app");
        app.set_terminal_viewport_rows(24);
        app.terminals[0].feed_output("e\u{301}x".as_bytes());
        app.begin_terminal_selection(0, 0);
        app.update_terminal_selection(1, 0);
        app.finish_terminal_selection();

        assert_eq!(app.terminal_selection_text().as_deref(), Some("e\u{301}"));
        let styled = app.terminal_screen_styled_lines();
        assert!(styled[0][0].1.inverse);
        assert!(styled[0][1].1.inverse);
        assert!(!styled[0][2].1.inverse);
    }

    #[test]
    fn terminal_selection_is_invalidated_when_scrolling_or_switching_tabs() {
        let mut app = App::new(&[]).expect("welcome app");
        app.set_terminal_viewport_rows(24);
        app.terminals[0].feed_output(b"select me");
        app.begin_terminal_selection(0, 0);
        app.update_terminal_selection(6, 0);
        app.finish_terminal_selection();
        assert_eq!(app.terminal_selection_text().as_deref(), Some("select"));

        app.scroll_terminal(1);
        assert!(app.terminal_selection_text().is_none());

        app.begin_terminal_selection(0, 0);
        app.update_terminal_selection(6, 0);
        app.finish_terminal_selection();
        app.open_terminal();
        assert!(app.terminal_selection_text().is_none());

        app.begin_terminal_selection(0, 0);
        app.update_terminal_selection(6, 0);
        app.finish_terminal_selection();
        app.switch_terminal(-1);
        assert!(app.terminal_selection_text().is_none());
    }

    #[test]
    fn ctrl_o_opens_relative_workspace_file() {
        let root = std::env::temp_dir().join(format!("mtide-open-file-{}", std::process::id()));
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&first, "first").expect("first");
        std::fs::write(&second, "second").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
            .expect("open picker");
        for character in "second.rs".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("path character");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open file");
        assert_eq!(app.file_lines(), &["second"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_shift_s_saves_to_a_new_path_without_overwriting() {
        let root = std::env::temp_dir().join(format!("mtide-save-as-{}", std::process::id()));
        let source = root.join("source.rs");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&source, "fn main() {}\n").expect("source");
        let mut app = App::new(&[source.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("open save as");
        for _ in "source.rs".chars() {
            app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
                .expect("clear path");
        }
        for character in "copy.rs".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("path character");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("save as");
        assert!(root.join("copy.rs").is_file());
        assert_eq!(app.buffer().path(), Some(root.join("copy.rs").as_path()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_g_jumps_to_line_and_column() {
        let path = std::env::temp_dir().join(format!("mtide-goto-{}", std::process::id()));
        std::fs::write(&path, "one\ntwo\nthree").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .expect("open goto");
        for character in "3:2".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("goto input");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("jump");
        assert_eq!(app.cursor(), Position { line: 2, column: 1 });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn overlays_restore_the_focus_that_opened_them() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Terminal;
        app.open_quick_open();
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close open file overlay");
        assert_eq!(app.focus(), Focus::Terminal);

        app.focus = Focus::Search;
        app.open_plugin_palette();
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close command palette");
        assert_eq!(app.focus(), Focus::Search);
    }

    #[test]
    fn registry_refresh_is_exposed_as_a_builtin_command() {
        assert!(builtin_commands()
            .iter()
            .any(|(id, _)| id == "plugin.refreshRegistry"));
    }

    #[test]
    fn output_end_navigates_to_last_registry_entry() {
        let mut app = App::new(&[]).expect("app");
        app.plugin_registry_entries = (0..12)
            .map(|index| PluginRegistryEntry {
                name: format!("plugin-{index}"),
                version: "1.0.0".into(),
                url: "https://example.com/plugin.zip".into(),
                sha256: None,
            })
            .collect();
        app.change_diff.clear();
        app.history.clear();
        app.blame_lines.clear();
        app.focus = Focus::Output;
        app.handle_key(key(KeyCode::End, KeyModifiers::NONE))
            .expect("output end");
        assert_eq!(app.output_scroll(), 11);
    }

    #[test]
    fn output_down_is_clamped_to_last_registry_entry() {
        let mut app = App::new(&[]).expect("app");
        app.plugin_registry_entries = (0..3)
            .map(|index| PluginRegistryEntry {
                name: format!("plugin-{index}"),
                version: "1.0.0".into(),
                url: "https://example.com/plugin.zip".into(),
                sha256: None,
            })
            .collect();
        app.change_diff.clear();
        app.history.clear();
        app.blame_lines.clear();
        app.focus = Focus::Output;
        for _ in 0..8 {
            app.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
                .expect("output down");
        }
        assert_eq!(app.output_scroll(), 2);
    }

    #[test]
    fn command_palette_exposes_and_executes_builtin_commands() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["one".into(), "two".into()], None);
        app.buffer_mut().set_cursor(0, 0);
        app.open_plugin_palette();
        for character in "join lines".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("palette query");
        }
        assert_eq!(
            app.plugin_palette_items(),
            vec![("editor.joinLines".to_owned(), "Join Lines".to_owned())]
        );
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("execute builtin command");
        assert_eq!(app.file_lines(), &["one two"]);
        assert!(!app.plugin_palette_open());
    }

    #[test]
    fn diff_commands_toggle_shared_settings_and_context() {
        let config_path =
            std::env::temp_dir().join(format!("mtide-diff-options-{}.json", std::process::id()));
        let mut app = App::new(&[
            "--config".into(),
            config_path.to_string_lossy().into_owned(),
        ])
        .expect("app");
        assert!(app
            .execute_builtin_command("options.toggleDiffWordWrap")
            .expect("wrap command"));
        assert!(app.diff_word_wrap());
        assert!(app
            .execute_builtin_command("options.toggleDiffHighContrast")
            .expect("contrast command"));
        assert!(app.diff_high_contrast());
        assert!(app
            .execute_builtin_command("options.useFullFileDiff")
            .expect("context command"));
        assert!(app.diff_full_context());
        let saved = Settings::load_from_path(&config_path);
        assert!(saved.editor.diff_word_wrap);
        assert!(saved.editor.diff_high_contrast);
        assert_eq!(saved.editor.diff_context, "full");
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn settings_overlay_toggles_layout_values() {
        let mut app = App::new(&[]).expect("welcome app");
        assert!(app.sidebar_visible());
        app.open_plugin_palette();
        for character in "open settings".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("settings query");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open settings");
        assert!(app.settings_overlay_open());
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("toggle sidebar");
        assert!(!app.sidebar_visible());
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close settings");
        assert!(!app.settings_overlay_open());
    }

    #[test]
    fn bottom_panel_height_shortcuts_are_clamped_and_persisted_in_settings_state() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Output;
        assert_eq!(app.bottom_height(), 5);
        app.handle_key(key(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("grow bottom panel");
        assert_eq!(app.bottom_height(), 6);
        app.settings.bottom_height = 3;
        app.handle_key(key(KeyCode::Down, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("shrink bottom panel at minimum");
        assert_eq!(app.bottom_height(), 3);
        app.settings.bottom_height = 24;
        app.handle_key(key(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("grow bottom panel at maximum");
        assert_eq!(app.bottom_height(), 24);
    }

    #[test]
    fn settings_overlay_toggles_workspace_visibility_values() {
        let mut app = App::new(&[]).expect("welcome app");
        app.open_settings();
        app.settings_index = 11;
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.settings_index = 12;
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.settings_index = 13;
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.settings_index = 14;
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        app.settings_index = 15;
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.settings.quick_open.show_hidden);
        assert!(app.settings.quick_open.show_git_ignored);
        assert!(app.settings.explorer.show_hidden);
        assert!(app.settings.explorer.show_git_ignored);
        assert!(app.settings.explorer.auto_reveal);
        let rows = app.settings_rows();
        assert!(rows[11].1 && rows[12].1 && rows[13].1 && rows[14].1 && rows[15].1);
    }

    #[test]
    fn settings_overlay_controls_auto_save_and_delay() {
        let mut app = App::new(&[]).expect("welcome app");
        app.settings_overlay = true;
        app.settings_index = 17;
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.settings.editor.auto_save);
        app.settings_index = 18;
        app.handle_settings_key(key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.settings.editor.auto_save_delay_ms, 10_000);
        app.handle_settings_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.settings.editor.auto_save_delay_ms, 5_000);
    }

    #[test]
    fn settings_overlay_toggles_and_persists_selection_style() {
        let config_path =
            std::env::temp_dir().join(format!("mtide-selection-style-{}.json", std::process::id()));
        let mut app = App::new(&[]).expect("welcome app");
        app.settings_overlay = true;
        app.settings_index = 19;

        assert_eq!(app.settings.editor.selection_style, "background");
        app.handle_settings_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.settings.editor.selection_style, "invert");
        assert!(app.settings_rows()[19].1);
        app.settings
            .save_to_path(&config_path)
            .expect("save settings");

        let loaded = Settings::load_from_path(&config_path);
        assert_eq!(loaded.editor.selection_style, "invert");
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn explorer_auto_reveal_expands_and_selects_active_file() {
        let root = std::env::temp_dir().join(format!("mtide-auto-reveal-{}", std::process::id()));
        let file = root.join("src").join("nested").join("main.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("directories");
        std::fs::write(&file, "fn main() {}\n").expect("file");
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.settings.explorer.auto_reveal = true;
        app.open_buffer(Buffer::open(&file).expect("buffer"));
        assert!(app.explorer_expanded(&root));
        assert!(app.explorer_expanded(&root.join("src")));
        assert!(app.explorer_expanded(&root.join("src/nested")));
        assert_eq!(app.selected_explorer_path(), Some(file.clone()));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn selection_tab_indents_and_shift_tab_outdents_as_one_edit() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["one".into(), "two".into()], None);
        app.selection_anchor = Some(Position { line: 0, column: 0 });
        app.buffer_mut().set_cursor(1, 3);
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("indent selection");
        assert_eq!(app.file_lines(), &["    one", "    two"]);
        app.handle_key(key(KeyCode::BackTab, KeyModifiers::SHIFT))
            .expect("outdent selection");
        assert_eq!(app.file_lines(), &["one", "two"]);
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo outdent");
        assert_eq!(app.file_lines(), &["    one", "    two"]);
    }

    #[test]
    fn split_selection_into_lines_creates_unicode_safe_multiselection() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["a你好".into(), "中b".into(), "tail".into()], None);
        app.selection_anchor = Some(Position { line: 0, column: 1 });
        app.buffer_mut().set_cursor(2, 2);
        app.handle_key(key(
            KeyCode::Char('i'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ))
        .expect("split selection");
        assert_eq!(
            app.active_selection_ranges(),
            vec![
                (
                    Position { line: 0, column: 1 },
                    Position { line: 0, column: 3 }
                ),
                (
                    Position { line: 1, column: 0 },
                    Position { line: 1, column: 2 }
                ),
                (
                    Position { line: 2, column: 0 },
                    Position { line: 2, column: 2 }
                ),
            ]
        );
    }

    #[test]
    fn explorer_can_create_rename_and_delete_entries() {
        let root =
            std::env::temp_dir().join(format!("mtide-explorer-actions-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let source = root.join("source.rs");
        std::fs::write(&source, "fn main() {}\n").expect("source");
        let mut app = App::new(&[source.to_string_lossy().into_owned()]).expect("app");
        app.focus = Focus::Explorer;
        app.search_root = root.clone();
        app.refresh_explorer();

        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("new file mode");
        for character in "created.txt".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("file name");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("create file");
        assert!(root.join("created.txt").is_file());

        app.explorer_index = app
            .explorer_paths
            .iter()
            .position(|path| path == &root.join("created.txt"))
            .expect("created entry");
        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("rename mode");
        for _ in "created.txt".chars() {
            app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
                .expect("clear name");
        }
        for character in "renamed.txt".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("renamed name");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("rename file");
        assert!(root.join("renamed.txt").is_file());

        app.explorer_index = app
            .explorer_paths
            .iter()
            .position(|path| path == &source)
            .expect("source entry");
        app.language_overrides
            .insert(path_key(&source), "rust".to_owned());
        app.handle_key(key(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("rename open file mode");
        for _ in "source.rs".chars() {
            app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
                .expect("clear open file name");
        }
        for character in "moved.rs".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("new open file name");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("rename open file");
        assert_eq!(app.buffer().path(), Some(root.join("moved.rs").as_path()));
        assert!(root.join("moved.rs").is_file());
        assert_eq!(
            app.language_overrides
                .get(&path_key(&root.join("moved.rs"))),
            Some(&"rust".to_owned())
        );
        assert!(!app.language_overrides.contains_key(&path_key(&source)));

        app.handle_key(key(KeyCode::Char('N'), KeyModifiers::SHIFT))
            .expect("new directory mode");
        for character in "empty".chars() {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("directory name");
        }
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("create directory");
        app.explorer_index = app
            .explorer_paths
            .iter()
            .position(|path| path == &root.join("empty"))
            .expect("directory entry");
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("delete confirmation");
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("delete directory");
        assert!(!root.join("empty").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn editor_folding_hides_indented_lines_without_changing_buffer() {
        let path = std::env::temp_dir().join(format!("mtide-fold-{}", std::process::id()));
        std::fs::write(&path, "root\n  child\n  child2\nafter\n").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("fold");
        assert!(app.is_line_folded(0));
        assert!(app.is_line_hidden(1));
        assert!(app.is_line_hidden(2));
        assert_eq!(
            app.file_lines(),
            &["root", "  child", "  child2", "after", ""]
        );
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
            .expect("skip folded block");
        assert_eq!(app.cursor().line, 3);
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE))
            .expect("skip folded block upward");
        assert_eq!(app.cursor().line, 0);
        app.handle_key(key(
            KeyCode::Char(']'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("expand all");
        assert!(!app.is_line_hidden(1));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn editor_line_operations_are_atomic_and_selection_safe() {
        let mut app = App::new(&[]).expect("app");
        app.buffers[0] = Buffer::new(vec!["a".into(), "b".into(), "c".into()], None);
        app.buffer_mut().set_cursor(1, 0);
        app.selection_anchor = Some(Position { line: 1, column: 0 });
        app.handle_key(key(KeyCode::Up, KeyModifiers::ALT | KeyModifiers::SHIFT))
            .expect("move line");
        assert_eq!(app.file_lines(), &["b", "a", "c"]);
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo move");
        assert_eq!(app.file_lines(), &["a", "b", "c"]);

        app.buffer_mut().set_cursor(1, 0);
        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .expect("join lines");
        assert_eq!(app.file_lines(), &["a", "b c"]);
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .expect("undo join");
        assert_eq!(app.file_lines(), &["a", "b", "c"]);

        app.buffer_mut().set_cursor(1, 0);
        app.handle_key(key(
            KeyCode::Char('s'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ))
        .expect("sort lines");
        assert_eq!(app.file_lines(), &["a", "b", "c"]);
        app.handle_key(key(
            KeyCode::Char('u'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ))
        .expect("uppercase line");
        assert_eq!(app.file_lines(), &["a", "B", "c"]);
    }

    #[test]
    fn markdown_details_blocks_fold_as_a_single_region() {
        let path =
            std::env::temp_dir().join(format!("mtide-details-fold-{}.md", std::process::id()));
        std::fs::write(
            &path,
            "<details>\n<summary>More</summary>\n\nUse `</details>` literally.\ncontent\n</details>\nafter\n",
        )
        .expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("fold details");
        assert!(app.is_line_folded(0));
        assert!(app.is_line_hidden(1));
        assert!(app.is_line_hidden(5));
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE))
            .expect("skip details");
        assert_eq!(app.cursor().line, 6);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn markdown_details_fold_ignores_matching_and_multiline_code_span_delimiters() {
        let path = std::env::temp_dir().join(format!(
            "mtide-details-multiline-code-{}.md",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "<details>\n<summary>More</summary>\nUse ``literal `tick` and </details>\nstill code``\nbody\n</details>\nafter\n",
        )
        .expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("fold details");
        assert!(app.is_line_hidden(2));
        assert!(app.is_line_hidden(3));
        assert!(app.is_line_hidden(4));
        assert!(!app.is_line_hidden(6));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn markdown_details_tags_ignore_inline_code_and_fenced_code() {
        let mut state = MarkdownDetailsScanState::default();
        assert_eq!(markdown_details_tags("中文 </details>", &mut state), (0, 1));
        assert_eq!(
            markdown_details_tags("Use `</details>`", &mut state),
            (0, 0)
        );
        assert_eq!(markdown_details_tags("```html", &mut state), (0, 0));
        assert_eq!(markdown_details_tags("</details>", &mut state), (0, 0));
        assert_eq!(markdown_details_tags("```", &mut state), (0, 0));
        assert_eq!(markdown_details_tags("<details>", &mut state), (1, 0));
        assert_eq!(markdown_details_tags("</details>", &mut state), (0, 1));
    }

    #[test]
    fn markdown_details_scanner_matches_multiline_backtick_delimiters() {
        let mut state = MarkdownDetailsScanState::default();
        assert_eq!(markdown_details_tags("<details>", &mut state), (1, 0));
        assert_eq!(
            markdown_details_tags("Use ``literal `tick` and </details>", &mut state),
            (0, 0)
        );
        assert_eq!(
            markdown_details_tags("the close tag is still code``.", &mut state),
            (0, 0)
        );
        assert_eq!(markdown_details_tags("</details>", &mut state), (0, 1));

        assert_eq!(markdown_details_tags("<details>", &mut state), (1, 0));
        assert_eq!(
            markdown_details_tags("Use \\`</details>", &mut state),
            (0, 1)
        );
    }

    #[test]
    fn toggle_comment_reports_unknown_language_instead_of_guessing() {
        let path = std::env::temp_dir().join(format!("mtide-comment-{}.txt", std::process::id()));
        std::fs::write(&path, "value\n").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('/'), KeyModifiers::CONTROL))
            .expect("toggle comment");
        assert_eq!(app.file_lines(), &["value", ""]);
        assert!(app.status_message().contains("unavailable"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enter_inherits_indent_and_indents_block_bodies() {
        let path = std::env::temp_dir().join(format!("mtide-indent-{}", std::process::id()));
        std::fs::write(&path, "fn main() {").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.buffer_mut().move_end();
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("newline");
        assert_eq!(app.file_lines(), &["fn main() {", "    "]);
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("indented newline");
        assert_eq!(app.file_lines(), &["fn main() {", "    ", "    "]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enter_uses_detected_indent_when_editorconfig_is_absent() {
        let path =
            std::env::temp_dir().join(format!("mtide-detected-indent-{}", std::process::id()));
        std::fs::write(&path, "fn main() {\n  if true {").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.buffer_mut().set_cursor(1, 11);
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("newline");
        assert_eq!(app.file_lines(), &["fn main() {", "  if true {", "    "]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn settings_can_override_detected_indent_style() {
        let path =
            std::env::temp_dir().join(format!("mtide-indent-setting-{}", std::process::id()));
        std::fs::write(&path, "fn main() {\n  if true {").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        assert!(app.indent_summary().contains("auto"));
        app.settings_overlay = true;
        app.settings_index = 9;
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("toggle tabs");
        assert!(!app.auto_detect_indent);
        assert!(!app.settings.editor.auto_detect_indent);
        assert_eq!(app.indent_summary(), "Tabs");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn settings_can_cycle_manual_indent_size() {
        let mut app = App::new(&[]).expect("app");
        app.settings_overlay = true;
        app.settings_index = 10;
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("cycle indent size");
        assert!(!app.auto_detect_indent);
        assert_eq!(app.settings.editor.indent_size, 8);
        assert_eq!(app.indent_summary(), "Spaces:8");
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE))
            .expect("cycle indent size backwards");
        assert_eq!(app.settings.editor.indent_size, 4);
    }

    #[test]
    fn ctrl_shift_enter_replaces_across_workspace() {
        let root =
            std::env::temp_dir().join(format!("mtide-replace-workspace-{}", std::process::id()));
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&first, "Needle one\nNEEDLE two\n").expect("first");
        std::fs::write(&second, "second NeEdLe\n").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.focus = Focus::Search;
        app.replace_mode = true;
        app.search_query = "needle".to_owned();
        app.replace_query = "done".to_owned();
        app.refresh_search();
        app.handle_key(key(
            KeyCode::Enter,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .expect("replace workspace");
        assert_eq!(app.file_lines(), &["done one", "done two", ""]);
        assert!(app.buffer().dirty());
        assert_eq!(
            std::fs::read_to_string(&second).expect("second content"),
            "second done\n"
        );
        assert!(app.status_message().contains("3 occurrence(s)"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saves_workspace_relative_folders_and_removes_active_root() {
        let root = std::env::temp_dir().join(format!(
            "mtide-workspace-edit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("first folder");
        std::fs::create_dir_all(&second).expect("second folder");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.add_workspace_folder(&second).expect("add folder");
        let workspace = root.join("saved.ttt");
        app.save_workspace_file(&workspace).expect("save workspace");
        let saved: WorkspaceFile =
            serde_json::from_str(&std::fs::read_to_string(&workspace).expect("workspace file"))
                .expect("workspace json");
        assert_eq!(saved.folders.len(), 2);
        assert!(saved
            .folders
            .iter()
            .any(|folder| folder.path == Path::new("first")));
        assert!(saved
            .folders
            .iter()
            .any(|folder| folder.path == Path::new("second")));
        assert!(app.remove_active_workspace_folder().expect("remove folder"));
        assert_eq!(
            app.workspace_roots(),
            &[second.canonicalize().expect("second root")]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_result_navigation_wraps_in_both_directions() {
        let mut app = App::new(&[]).expect("welcome app");
        app.search_results = vec![
            SearchResult {
                path: PathBuf::from("one.rs"),
                line: 1,
                column: 1,
                text: "needle".to_owned(),
            },
            SearchResult {
                path: PathBuf::from("two.rs"),
                line: 2,
                column: 1,
                text: "needle".to_owned(),
            },
        ];
        app.focus = Focus::Search;
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .expect("next match");
        assert_eq!(app.search_index(), 1);
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .expect("wrap next match");
        assert_eq!(app.search_index(), 0);
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .expect("previous match");
        assert_eq!(app.search_index(), 1);
    }

    #[test]
    fn project_name_comes_from_first_workspace_root() {
        let root = std::env::temp_dir().join(format!("mtide-project-name-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("project root");
        let app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        assert_eq!(
            app.project_name(),
            root.file_name().unwrap().to_string_lossy()
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn file_path_label_prefers_workspace_relative_path() {
        let root = std::env::temp_dir().join(format!("mtide-file-label-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("directory");
        let file = root.join("src/main.rs");
        std::fs::write(&file, "fn main() {}").expect("file");
        let app = App::new(&[file.to_string_lossy().into_owned()]).expect("app");
        assert_eq!(app.file_path_label(), "main.rs");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn branch_creation_mode_captures_name_without_switching_focus() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Branches;
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("open branch input");
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE))
            .expect("branch character");
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::NONE))
            .expect("branch character");
        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE))
            .expect("branch home");
        app.handle_paste("中").expect("branch paste");
        assert!(app.branch_mode());
        assert_eq!(app.branch_name(), "中fe");
    }

    #[test]
    fn changes_tree_groups_workspace_paths_and_collapses_directories() {
        let first = std::env::temp_dir().join(format!("mtide-tree-first-{}", std::process::id()));
        let second = std::env::temp_dir().join(format!("mtide-tree-second-{}", std::process::id()));
        let mut app = App::new(&[]).expect("app");
        app.settings.git.file_view = "tree".to_owned();
        app.workspace_roots = vec![first.clone(), second.clone()];
        app.changes = vec![
            GitChange {
                index: ' ',
                worktree: 'M',
                path: "src/lib.rs".into(),
            },
            GitChange {
                index: 'A',
                worktree: ' ',
                path: "src/main.rs".into(),
            },
            GitChange {
                index: '?',
                worktree: '?',
                path: "README.md".into(),
            },
            GitChange {
                index: ' ',
                worktree: 'M',
                path: "src/lib.rs".into(),
            },
        ];
        app.change_roots = vec![first.clone(), first.clone(), first.clone(), second.clone()];

        let rows = app.changes_tree_rows();
        assert!(rows.iter().any(|row| row.key
            == format!(
                "dir:changes:{}",
                first.file_name().unwrap().to_string_lossy()
            )));
        assert!(rows.iter().any(|row| row.file_index == Some(3)));
        let first_root_src = rows
            .iter()
            .position(|row| {
                row.key
                    == format!(
                        "dir:changes:{}/src",
                        first.file_name().unwrap().to_string_lossy()
                    )
            })
            .expect("first workspace src folder");
        let first_root_row = rows
            .iter()
            .position(|row| {
                row.key
                    == format!(
                        "dir:changes:{}",
                        first.file_name().unwrap().to_string_lossy()
                    )
            })
            .expect("first workspace root");
        assert_eq!(rows[first_root_src].descendant_files, 1);
        app.activate_changes_tree_row(first_root_row);
        assert!(app
            .changes_tree_rows()
            .iter()
            .any(|row| row.key == rows[first_root_row].key && !row.expanded));
        app.handle_changes_key(key(KeyCode::Right, KeyModifiers::NONE))
            .expect("expand selected workspace directory");
        assert!(app
            .changes_tree_rows()
            .iter()
            .any(|row| row.key == rows[first_root_row].key && row.expanded));
        let _ = std::fs::remove_dir_all(first);
        let _ = std::fs::remove_dir_all(second);
    }

    #[test]
    fn changes_rows_separate_staged_and_unstaged_sides_of_same_file() {
        let mut app = App::new(&[]).expect("app");
        app.changes = vec![
            GitChange {
                index: 'M',
                worktree: ' ',
                path: "src/staged.rs".into(),
            },
            GitChange {
                index: ' ',
                worktree: 'M',
                path: "src/unstaged.rs".into(),
            },
            GitChange {
                index: 'M',
                worktree: 'M',
                path: "src/mixed.rs".into(),
            },
            GitChange {
                index: '?',
                worktree: '?',
                path: "new.rs".into(),
            },
        ];
        let rows = app.changes_tree_rows();
        let staged = rows
            .iter()
            .find(|row| {
                row.kind == ChangeTreeRowKind::Section && row.group == Some(ChangeGroup::Staged)
            })
            .expect("staged section");
        let changes = rows
            .iter()
            .find(|row| {
                row.kind == ChangeTreeRowKind::Section && row.group == Some(ChangeGroup::Changes)
            })
            .expect("changes section");
        assert_eq!(staged.descendant_files, 2);
        assert_eq!(changes.descendant_files, 3);
        assert_eq!(
            rows.iter().filter(|row| row.file_index == Some(2)).count(),
            2,
            "a file with staged and unstaged edits appears in both sections"
        );
    }

    #[test]
    fn changes_section_stage_and_unstage_affect_only_matching_changes() {
        let root =
            std::env::temp_dir().join(format!("mtide-git-section-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git")
        };
        assert!(git(&["init", "-q"]).status.success());
        assert!(git(&["config", "user.email", "mtide@example.invalid"])
            .status
            .success());
        assert!(git(&["config", "user.name", "mtide"]).status.success());
        for file in ["staged.rs", "unstaged.rs", "mixed.rs"] {
            std::fs::write(root.join(file), "base\n").expect("initial file");
        }
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-qm", "initial"]).status.success());
        std::fs::write(root.join("staged.rs"), "staged\n").expect("staged content");
        assert!(git(&["add", "staged.rs"]).status.success());
        std::fs::write(root.join("unstaged.rs"), "unstaged\n").expect("unstaged content");
        std::fs::write(root.join("mixed.rs"), "staged part\n").expect("mixed staged content");
        assert!(git(&["add", "mixed.rs"]).status.success());
        std::fs::write(root.join("mixed.rs"), "unstaged part\n").expect("mixed worktree content");

        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        for _ in 0..100 {
            app.poll_background();
            if !app.git_state_pending && app.changes.len() == 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.focus = Focus::Changes;
        app.changes_tree_selection = Some("section:changes".to_owned());
        app.handle_changes_key(key(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("stage changes section");
        for _ in 0..100 {
            app.poll_background();
            if !app.git_state_pending
                && app
                    .changes
                    .iter()
                    .all(|change| change.index != ' ' && change.worktree == ' ')
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app
            .changes
            .iter()
            .all(|change| change.index != ' ' && change.worktree == ' '));

        app.changes_tree_selection = Some("section:staged".to_owned());
        app.handle_changes_key(key(KeyCode::Char('u'), KeyModifiers::NONE))
            .expect("unstage staged section");
        for _ in 0..100 {
            app.poll_background();
            if !app.git_state_pending && app.changes.iter().all(|change| change.index == ' ') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.changes.iter().all(|change| change.index == ' '));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changes_panel_stages_and_unstages_all_files() {
        let root = std::env::temp_dir().join(format!("mtide-git-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git")
        };
        assert!(git(&["init", "-q"]).status.success());
        assert!(git(&["config", "user.email", "mtide@example.invalid"])
            .status
            .success());
        assert!(git(&["config", "user.name", "mtide"]).status.success());
        std::fs::write(root.join("a.txt"), "a\n").expect("a");
        std::fs::write(root.join("b.txt"), "b\n").expect("b");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "initial"]).status.success());
        std::fs::write(root.join("a.txt"), "changed a\n").expect("a change");
        std::fs::write(root.join("b.txt"), "changed b\n").expect("b change");

        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.focus = Focus::Changes;
        app.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE))
            .expect("stage all");
        for _ in 0..100 {
            app.poll_background();
            if app.changes.iter().all(|change| change.index != ' ') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.changes.iter().all(|change| change.index != ' '));
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("toggle selected file off stage");
        for _ in 0..100 {
            app.poll_background();
            if app
                .changes
                .first()
                .is_some_and(|change| change.index == ' ')
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.changes.first().map(|change| change.index), Some(' '));
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE))
            .expect("toggle selected file on stage");
        for _ in 0..100 {
            app.poll_background();
            if app
                .changes
                .first()
                .is_some_and(|change| change.index != ' ')
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app
            .changes
            .first()
            .is_some_and(|change| change.index != ' '));
        app.handle_key(key(KeyCode::Char('A'), KeyModifiers::SHIFT))
            .expect("unstage all");
        for _ in 0..100 {
            app.poll_background();
            if app.changes.iter().all(|change| change.index == ' ') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.changes.iter().all(|change| change.index == ' '));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changes_file_view_toggles_and_persists_git_setting() {
        let root = std::env::temp_dir().join(format!("mtide-git-file-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let config = root.join("settings.json");
        std::fs::write(&config, "{}").expect("settings");
        let args = vec![
            "--config".to_owned(),
            config.to_string_lossy().into_owned(),
            "--welcome".to_owned(),
        ];
        let mut app = App::new(&args).expect("app");
        app.changes = vec![GitChange {
            index: ' ',
            worktree: 'M',
            path: "src/lib.rs".into(),
        }];
        assert_eq!(app.changes_file_view(), "list");
        assert_eq!(app.changes_tree_rows()[0].key, "section:changes");
        assert_eq!(app.changes_tree_rows()[1].label, "src/lib.rs");

        app.handle_changes_key(key(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("switch to tree");
        assert_eq!(app.changes_file_view(), "tree");
        assert_eq!(app.changes_tree_rows()[0].key, "section:changes");
        assert_eq!(app.changes_tree_rows()[1].key, "dir:changes:src");
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).expect("saved settings"))
                .expect("settings json");
        assert_eq!(saved["git"]["fileView"], "tree");

        app.handle_changes_key(key(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("switch to list");
        assert_eq!(app.changes_file_view(), "list");
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).expect("saved settings"))
                .expect("settings json");
        assert_eq!(saved["git"]["fileView"], "list");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_file_resolves_all_relative_folders() {
        let root = std::env::temp_dir().join(format!("mtide-workspace-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("first folder");
        std::fs::create_dir_all(&second).expect("second folder");
        let workspace = root.join("project.ttt");
        std::fs::write(
            &workspace,
            r#"{"folders":[{"path":"first"},{"path":"second"}]}"#,
        )
        .expect("workspace file");
        let target = TargetSpec {
            path: workspace,
            line: 0,
            column: 0,
        };
        let roots = workspace_roots(Some(&target)).expect("workspace roots");
        assert_eq!(
            roots,
            vec![
                first.canonicalize().unwrap(),
                second.canonicalize().unwrap()
            ]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn multi_root_explorer_refresh_keeps_all_workspace_folders() {
        let root =
            std::env::temp_dir().join(format!("mtide-explorer-workspace-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("first folder");
        std::fs::create_dir_all(&second).expect("second folder");
        let first_file = first.join("one.rs");
        let second_file = second.join("two.rs");
        std::fs::write(&first_file, "one").expect("first file");
        std::fs::write(&second_file, "two").expect("second file");
        let workspace = root.join("project.ttt");
        std::fs::write(
            &workspace,
            r#"{"folders":[{"path":"first"},{"path":"second"}]}"#,
        )
        .expect("workspace");
        let mut app = App::new(&[workspace.to_string_lossy().into_owned()]).expect("app");
        app.refresh_explorer();
        assert!(app.explorer_paths().contains(&first_file));
        assert!(app.explorer_paths().contains(&second_file));
        assert_eq!(app.explorer_label(&first_file), "first/one.rs");
        assert_eq!(app.explorer_label(&second_file), "second/two.rs");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn running_editor_adds_forwarded_directory_to_workspace_roots() {
        let root =
            std::env::temp_dir().join(format!("mtide-forward-folder-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        std::fs::write(second.join("README.md"), "second").expect("second file");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_external_target(&second.to_string_lossy())
            .expect("add folder");
        for _ in 0..100 {
            app.poll_background();
            if app
                .git_changes_by_root
                .contains_key(&second.canonicalize().expect("canonical cache"))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.workspace_roots().len(), 2);
        assert!(app
            .git_changes_by_root
            .contains_key(&second.canonicalize().expect("canonical cache")));
        assert!(app
            .workspace_roots()
            .contains(&second.canonicalize().expect("canonical")));
        assert!(app
            .explorer_paths()
            .iter()
            .any(|path| path.starts_with(&second)));
        app.open_external_target(&second.to_string_lossy())
            .expect("duplicate folder");
        assert_eq!(app.workspace_roots().len(), 2);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn changes_tree_directory_stage_only_affects_its_descendants() {
        let root =
            std::env::temp_dir().join(format!("mtide-git-tree-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("src directory");
        std::fs::create_dir_all(root.join("docs")).expect("docs directory");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git")
        };
        assert!(git(&["init", "-q"]).status.success());
        assert!(git(&["config", "user.email", "mtide@example.invalid"])
            .status
            .success());
        assert!(git(&["config", "user.name", "mtide"]).status.success());
        for file in ["src/a.rs", "src/b.rs", "docs/readme.md"] {
            std::fs::write(root.join(file), "original\n").expect("initial file");
        }
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-qm", "initial"]).status.success());
        for file in ["src/a.rs", "src/b.rs", "docs/readme.md"] {
            std::fs::write(root.join(file), "changed\n").expect("changed file");
        }

        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.settings.git.file_view = "tree".to_owned();
        app.focus = Focus::Changes;
        app.changes_tree_selection = Some("dir:changes:src".to_owned());
        app.handle_changes_key(key(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("stage src directory");
        for _ in 0..100 {
            app.poll_background();
            if !app.git_state_pending {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let status: BTreeMap<_, _> = app
            .changes
            .iter()
            .map(|change| (change.path.as_str(), change.index))
            .collect();
        assert_ne!(status.get("src/a.rs"), Some(&' '));
        assert_ne!(status.get("src/b.rs"), Some(&' '));
        assert_eq!(status.get("docs/readme.md"), Some(&' '));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changes_panel_aggregates_new_workspace_root_git_status() {
        let root =
            std::env::temp_dir().join(format!("mtide-git-multi-root-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        let git = std::process::Command::new("git")
            .args(["-C", &second.to_string_lossy(), "init", "-q"])
            .status()
            .expect("git");
        assert!(git.success());
        let git_run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&second)
                .status()
                .expect("git command")
        };
        assert!(git_run(&["config", "user.email", "mtide@example.invalid"]).success());
        assert!(git_run(&["config", "user.name", "mtide"]).success());
        std::fs::write(second.join("new.rs"), "fn old_file() {}\n").expect("new file");
        assert!(git_run(&["add", "new.rs"]).success());
        assert!(git_run(&["commit", "-q", "-m", "initial"]).success());
        std::fs::write(second.join("new.rs"), "fn new_file() {}\n").expect("changed file");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_external_target(&second.to_string_lossy())
            .expect("add second root");
        for _ in 0..100 {
            app.poll_background();
            if app.changes.iter().any(|change| change.path == "new.rs") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let index = app
            .changes()
            .iter()
            .position(|change| change.path == "new.rs")
            .expect("second root change");
        assert_eq!(app.change_label(index), "second/new.rs");
        assert_eq!(
            app.change_root(index),
            second.canonicalize().expect("canonical")
        );
        app.change_index = index;
        app.focus = Focus::Changes;
        app.handle_changes_key(key(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("stage second root change");
        for _ in 0..100 {
            app.poll_background();
            if !app.git_state_pending {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let staged = std::process::Command::new("git")
            .args([
                "-C",
                &second.to_string_lossy(),
                "diff",
                "--cached",
                "--name-only",
            ])
            .output()
            .expect("git staged status");
        assert!(String::from_utf8_lossy(&staged.stdout).contains("new.rs"));
        app.handle_changes_key(key(KeyCode::Char('u'), KeyModifiers::NONE))
            .expect("unstage second root change");
        let unstaged = std::process::Command::new("git")
            .args([
                "-C",
                &second.to_string_lossy(),
                "diff",
                "--cached",
                "--name-only",
            ])
            .output()
            .expect("git unstaged status");
        assert!(!String::from_utf8_lossy(&unstaged.stdout).contains("new.rs"));
        drop(app);
        let mut cleaned = false;
        for _ in 0..100 {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {
                    cleaned = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    cleaned = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("cleanup: {error}"),
            }
        }
        assert!(cleaned, "workspace cleanup timed out");
    }

    #[test]
    fn active_file_switches_branch_context_to_its_workspace_root() {
        let root =
            std::env::temp_dir().join(format!("mtide-git-branch-context-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        for (directory, branch) in [(&first, "first-main"), (&second, "second-main")] {
            let git = |args: &[&str]| {
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(directory)
                    .status()
                    .expect("git")
            };
            assert!(git(&["init", "-q"]).success());
            assert!(git(&["config", "user.email", "mtide@example.invalid"]).success());
            assert!(git(&["config", "user.name", "mtide"]).success());
            std::fs::write(directory.join("main.rs"), "fn main() {}\n").expect("file");
            assert!(git(&["add", "main.rs"]).success());
            assert!(git(&["commit", "-q", "-m", "initial"]).success());
            assert!(git(&["branch", "-M", branch]).success());
        }
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_external_target(&second.to_string_lossy())
            .expect("add second root");
        app.open_external_target(&second.join("main.rs").to_string_lossy())
            .expect("open second file");
        assert_eq!(app.branch(), Some("second-main"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn explorer_git_status_resolves_paths_relative_to_search_root() {
        let root =
            std::env::temp_dir().join(format!("mtide-explorer-status-{}", std::process::id()));
        let file = root.join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("directories");
        std::fs::write(&file, "fn main() {}\n").expect("file");
        let mut app = App::new(&[root.to_string_lossy().into_owned()]).expect("app");
        app.changes = vec![GitChange {
            index: ' ',
            worktree: 'M',
            path: "src/main.rs".into(),
        }];
        assert_eq!(app.explorer_git_status(&file), Some((' ', 'M')));
        assert_eq!(app.explorer_git_status(&root.join("missing.rs")), None);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn explorer_git_status_uses_the_matching_multi_root_cache() {
        let root = std::env::temp_dir().join(format!(
            "mtide-explorer-status-roots-{}",
            std::process::id()
        ));
        let first = root.join("first");
        let second = root.join("second");
        let file = second.join("main.rs");
        std::fs::create_dir_all(&first).expect("first");
        std::fs::create_dir_all(&second).expect("second");
        std::fs::write(&file, "fn main() {}\n").expect("file");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.add_workspace_folder(&second).expect("add root");
        app.git_changes_by_root.insert(
            second.canonicalize().expect("canonical"),
            vec![GitChange {
                index: 'A',
                worktree: ' ',
                path: "main.rs".into(),
            }],
        );
        assert_eq!(app.explorer_git_status(&file), Some(('A', ' ')));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn extracts_completion_labels_and_hover_text() {
        let result = serde_json::json!({
            "items": [
                {"label":"alpha", "insertText":"alpha()"},
                {"label":"beta", "textEdit":{"newText":"beta<T>", "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}}}}
            ],
            "contents": [{"value":"fn alpha()"}]
        });
        assert_eq!(
            completion_items(&result),
            vec![
                LspCompletionItem {
                    label: "alpha".into(),
                    insert_text: "alpha()".into(),
                    edit_range: None,
                    additional_edits: Vec::new(),
                    commit_characters: Vec::new(),
                    resolve_params: None,
                },
                LspCompletionItem {
                    label: "beta".into(),
                    insert_text: "beta<T>".into(),
                    edit_range: Some(LspCompletionRange {
                        start_line: 0,
                        start_character: 1,
                        end_line: 0,
                        end_character: 3,
                    }),
                    additional_edits: Vec::new(),
                    commit_characters: Vec::new(),
                    resolve_params: None,
                },
            ]
        );
        assert_eq!(hover_text(&result), "fn alpha()");
    }

    #[test]
    fn preserves_completion_item_data_for_resolve_request() {
        let result = serde_json::json!({
            "items": [{
                "label": "detail",
                "data": {"source": "language-server", "id": 7}
            }]
        });
        let items = completion_items(&result);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].resolve_params,
            Some(serde_json::json!({
                "label": "detail",
                "data": {"source": "language-server", "id": 7}
            }))
        );
    }

    #[test]
    fn parses_insert_replace_completion_edit_using_replace_range() {
        let result = serde_json::json!({
            "items": [{
                "label": "completion",
                "textEdit": {
                    "newText": "completion",
                    "insert": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}},
                    "replace": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 5}}
                }
            }]
        });
        let items = completion_items(&result);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].edit_range,
            Some(LspCompletionRange {
                start_line: 0,
                start_character: 1,
                end_line: 0,
                end_character: 5,
            })
        );
    }

    #[test]
    fn parses_single_character_completion_commit_characters() {
        let items = completion_items(&serde_json::json!({
            "items": [{"label": "method", "commitCharacters": [".", "(", ".."]}]
        }));
        assert_eq!(items[0].commit_characters, vec!['.', '(']);
    }

    #[test]
    fn expands_lsp_snippets_without_leaking_tabstops() {
        assert_eq!(
            expand_lsp_snippet("fn ${1:name}($2) { $0 }"),
            "fn name() {  }"
        );
        assert_eq!(expand_lsp_snippet("${10:value} $10"), "value ");
        assert_eq!(expand_lsp_snippet("${1|one,two|}"), "one");
        assert_eq!(expand_lsp_snippet(r#"\${1:literal}"#), "${1:literal}");
    }

    #[test]
    fn merges_completion_resolve_details_without_losing_original_edit() {
        let original = serde_json::json!({
            "label": "item",
            "insertText": "item()",
            "textEdit": {
                "newText": "item()",
                "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 4}}
            },
            "data": {"id": 1}
        });
        let merged =
            merge_completion_item(&original, &serde_json::json!({"documentation": "detail"}));
        assert_eq!(merged["documentation"], "detail");
        assert_eq!(merged["textEdit"], original["textEdit"]);
        assert_eq!(merged["data"], original["data"]);
    }

    #[test]
    fn completion_applies_additional_text_edits_from_back_to_front() {
        let mut app = App::new_with_stdin(&[], Some("abc\nuse old;\n")).expect("app");
        app.apply_completion_item(&LspCompletionItem {
            label: "value".into(),
            insert_text: "value".into(),
            edit_range: Some(LspCompletionRange {
                start_line: 0,
                start_character: 3,
                end_line: 0,
                end_character: 3,
            }),
            additional_edits: vec![LspCompletionTextEdit {
                range: LspCompletionRange {
                    start_line: 1,
                    start_character: 4,
                    end_line: 1,
                    end_character: 7,
                },
                new_text: "new".into(),
            }],
            commit_characters: Vec::new(),
            resolve_params: None,
        });
        assert_eq!(app.file_lines(), vec!["abcvalue", "use new;", ""]);
    }

    #[test]
    fn applies_completion_text_edit_range_using_utf16_columns() {
        let mut app = App::new_with_stdin(&[], Some("你ab")).expect("app");
        app.apply_completion_item(&LspCompletionItem {
            label: "replacement".into(),
            insert_text: "X".into(),
            edit_range: Some(LspCompletionRange {
                start_line: 0,
                start_character: 1,
                end_line: 0,
                end_character: 3,
            }),
            additional_edits: Vec::new(),
            commit_characters: Vec::new(),
            resolve_params: None,
        });
        assert_eq!(app.file_lines(), vec!["你X"]);
    }

    #[test]
    fn normalizes_lsp_code_action_command_variants() {
        assert_eq!(
            lsp_execute_command_params(&serde_json::json!("quick.fix")),
            Some(serde_json::json!({"command":"quick.fix","arguments":[]}))
        );
        assert_eq!(
            lsp_execute_command_params(&serde_json::json!({
                "title": "Fix",
                "command": "quick.fix",
                "arguments": [{"uri": "file:///tmp/a.rs"}]
            })),
            Some(serde_json::json!({
                "command":"quick.fix",
                "arguments":[{"uri":"file:///tmp/a.rs"}]
            }))
        );
        assert_eq!(
            lsp_execute_command_params(&serde_json::json!({"title":"Missing command"})),
            None
        );
    }

    #[test]
    fn extracts_signature_help_with_active_parameter() {
        let result = serde_json::json!({
            "activeSignature": 0,
            "activeParameter": 1,
            "signatures": [{
                "label": "join(left, right)",
                "documentation": {"value": "Joins two paths."}
            }]
        });
        assert_eq!(
            signature_help_text(&result),
            "join(left, right)\nActive parameter: 2\nJoins two paths."
        );
    }

    #[test]
    fn parses_definition_location() {
        let result = serde_json::json!({
            "uri": "file:///tmp/lib.rs",
            "range": {"start": {"line": 4, "character": 3}, "end": {"line": 4, "character": 8}}
        });
        assert_eq!(
            definition_location(&result),
            Some((PathBuf::from("/tmp/lib.rs"), 4, 3))
        );
    }

    #[test]
    fn parses_multiple_lsp_locations() {
        let result = serde_json::json!([
            {"uri":"file:///tmp/a.rs","range":{"start":{"line":1,"character":2}}},
            {"uri":"file:///tmp/b.rs","range":{"start":{"line":3,"character":4}}}
        ]);
        assert_eq!(
            definition_locations(&result),
            vec![
                (PathBuf::from("/tmp/a.rs"), 1, 2),
                (PathBuf::from("/tmp/b.rs"), 3, 4)
            ]
        );
    }

    #[test]
    fn parses_flat_and_nested_document_symbols() {
        let default_path = PathBuf::from("/tmp/main.rs");
        let result = serde_json::json!([
            {
                "name": "main",
                "kind": 12,
                "range": {"start": {"line": 0, "character": 0}},
                "children": [{
                    "name": "helper",
                    "kind": 12,
                    "range": {"start": {"line": 2, "character": 4}}
                }]
            },
            {
                "name": "external",
                "kind": 12,
                "location": {
                    "uri": "file:///tmp/lib.rs",
                    "range": {"start": {"line": 5, "character": 2}}
                }
            }
        ]);
        assert_eq!(
            document_symbols(&result, &default_path),
            vec![
                ("main".into(), PathBuf::from("/tmp/main.rs"), 0, 0),
                ("helper".into(), PathBuf::from("/tmp/main.rs"), 2, 4),
                ("external".into(), PathBuf::from("/tmp/lib.rs"), 5, 2)
            ]
        );
    }

    #[test]
    fn applies_lsp_workspace_edit_in_reverse_order() {
        let mut buffer = Buffer::new(vec!["foo foo".into()], None);
        let changed = apply_lsp_text_edits(
            &mut buffer,
            "file:///tmp/test.rs",
            &json!({"changes":{"file:///tmp/test.rs":[
                {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"bar"},
                {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":7}},"newText":"bar"}
            ]}}),
        );
        assert_eq!(changed, 2);
        assert_eq!(buffer.lines(), &["bar bar"]);
    }

    #[test]
    fn lsp_text_edits_convert_utf16_columns_to_editor_columns() {
        let line = "a😀中";
        assert_eq!(utf16_column(line, 0), 0);
        assert_eq!(utf16_column(line, 2), 3);
        assert_eq!(editor_column_from_utf16(line, 3), Some(2));
        assert_eq!(editor_column_from_utf16(line, 2), None);

        let mut buffer = Buffer::new(vec![line.into()], None);
        let changed = apply_lsp_text_edits(
            &mut buffer,
            "file:///tmp/unicode.rs",
            &json!([{
                "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}},
                "newText": "X"
            }]),
        );
        assert_eq!(changed, 1);
        assert_eq!(buffer.lines(), &["aX中"]);
    }

    #[test]
    fn summarizes_lsp_workspace_edit_by_file() {
        let summary = workspace_edit_summary(&json!({
            "changes": {
                "file:///tmp/b.rs": [{"range": {}, "newText": "x"}],
                "file:///tmp/a.rs": [
                    {"range": {}, "newText": "x"},
                    {"range": {}, "newText": "y"}
                ]
            }
        }));
        assert_eq!(
            summary,
            vec!["  2 edit(s)  /tmp/a.rs", "  1 edit(s)  /tmp/b.rs"]
        );
    }

    #[test]
    fn summarizes_existing_resource_targets_as_conflicts() {
        let summary = workspace_edit_summary(&json!({
            "documentChanges": [{
                "kind": "create",
                "uri": "file:///tmp"
            }]
        }));
        assert_eq!(
            summary,
            vec!["! CONFLICT  resource target already exists: /tmp"]
        );
    }

    #[test]
    fn resource_conflict_preview_respects_overwrite_and_missing_sources() {
        let missing = format!("/tmp/mtide-missing-rename-source-{}", std::process::id());
        let summary = workspace_edit_summary(&json!({
            "documentChanges": [
                {"kind": "create", "uri": "file:///tmp", "options": {"overwrite": true}},
                {"kind": "rename", "oldUri": format!("file://{missing}"), "newUri": "file:///tmp", "options": {"overwrite": true}}
            ]
        }));
        assert_eq!(
            summary,
            vec![format!(
                "! CONFLICT  rename source does not exist: {missing}"
            )]
        );
    }

    #[test]
    fn rename_preview_keeps_conflict_visible_when_apply_fails() {
        let mut app = App::new(&[]).expect("welcome app");
        let edit = json!({
            "documentChanges": [{
                "kind": "create",
                "uri": "file:///tmp",
                "options": {"overwrite": false}
            }]
        });
        app.lsp_popup = Some(LspPopup::RenamePreview {
            uri: "file:///tmp/current.rs".to_owned(),
            edit,
            summary: vec!["! CONFLICT  target exists".to_owned()],
        });

        assert!(app
            .handle_lsp_popup_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("rename preview key"));
        assert!(matches!(
            app.lsp_popup,
            Some(LspPopup::RenamePreview { .. })
        ));
        assert!(app.status_message().contains("not applied"));
    }

    #[test]
    fn rejects_overlapping_workspace_edits_before_mutating() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["abcdef".into()], None);
        let edit = json!({
            "changes": {"file:///tmp/current.rs": [
                {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"x"},
                {"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":4}},"newText":"y"}
            ]}
        });
        assert!(app
            .apply_workspace_edit("file:///tmp/current.rs", &edit)
            .is_err());
        assert_eq!(app.file_lines(), &["abcdef"]);
    }

    #[test]
    fn applies_workspace_edit_returned_by_execute_command() {
        let path = std::env::temp_dir().join(format!(
            "mtide-execute-command-edit-{}.rs",
            std::process::id()
        ));
        std::fs::write(&path, "old\n").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        let uri = path_to_uri(&path);
        app.handle_lsp_execute_command_result(
            &uri,
            json!({
                "changes": {
                    uri.clone(): [{
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                        "newText": "new"
                    }]
                }
            }),
        )
        .expect("execute command edit");
        assert_eq!(app.file_lines(), &["new", ""]);
        assert!(app.status_message().contains("applied 1 edit"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn workspace_edit_updates_open_non_active_buffer_without_overwriting_disk() {
        let root =
            std::env::temp_dir().join(format!("mtide-open-workspace-edit-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::write(&first, "first").expect("first");
        std::fs::write(&second, "second").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("local edit");
        app.switch_tab(-1);

        let second_uri = path_to_uri(&second);
        let edit = json!({
            "changes": {
                second_uri: [{
                    "range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                    "newText": "Y"
                }]
            }
        });
        assert_eq!(
            app.apply_workspace_edit(&path_to_uri(&first), &edit)
                .expect("workspace edit"),
            1
        );
        assert_eq!(app.buffers[1].lines(), &["xYsecond"]);
        assert_eq!(
            std::fs::read_to_string(&second).expect("disk file"),
            "second"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_edits_share_one_undo_checkpoint() {
        let mut app = App::new(&[]).expect("welcome app");
        app.buffers[0] = Buffer::new(vec!["foo foo".into()], None);
        let edit = json!({
            "changes": {"file:///tmp/current.rs": [
                {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"bar"},
                {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":7}},"newText":"bar"}
            ]}
        });
        assert_eq!(
            app.apply_workspace_edit("file:///tmp/current.rs", &edit)
                .expect("workspace edit"),
            2
        );
        assert_eq!(app.file_lines(), &["bar bar"]);
        app.buffer_mut().undo();
        assert_eq!(app.file_lines(), &["foo foo"]);
    }

    #[test]
    fn applies_lsp_workspace_resource_operations_with_preflight() {
        let root = std::env::temp_dir().join(format!("mtide-lsp-resources-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let source = root.join("source.txt");
        let renamed = root.join("renamed.txt");
        let created = root.join("created.txt");
        std::fs::write(&source, "source").expect("source");
        let mut app = App::new(&[]).expect("welcome app");
        let edit = json!({
            "documentChanges": [
                {"kind":"create", "uri": format!("file://{}", created.display())},
                {"kind":"rename", "oldUri": format!("file://{}", source.display()), "newUri": format!("file://{}", renamed.display())},
                {"kind":"delete", "uri": format!("file://{}", created.display())}
            ]
        });
        assert_eq!(
            app.apply_workspace_edit("file:///tmp/current.rs", &edit)
                .expect("resources"),
            3
        );
        assert!(!source.exists());
        assert!(renamed.is_file());
        assert!(!created.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lsp_rename_file_migrates_the_active_buffer_path() {
        let root =
            std::env::temp_dir().join(format!("mtide-lsp-active-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("directory");
        let source = root.join("source.rs");
        let renamed = root.join("renamed.rs");
        std::fs::write(&source, "fn main() {}\n").expect("source");
        let mut app = App::new(&[source.to_string_lossy().into_owned()]).expect("app");
        let current_uri = format!("file://{}", source.display());
        let edit = json!({
            "documentChanges": [{
                "kind": "rename",
                "oldUri": current_uri,
                "newUri": format!("file://{}", renamed.display())
            }]
        });
        assert_eq!(
            app.apply_workspace_edit(&format!("file://{}", source.display()), &edit)
                .expect("rename active resource"),
            1
        );
        assert_eq!(app.buffer().path(), Some(renamed.as_path()));
        assert!(renamed.is_file());
        assert!(!source.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lsp_delete_file_marks_active_buffer_deleted_without_resyncing_old_uri() {
        let root =
            std::env::temp_dir().join(format!("mtide-lsp-active-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("directory");
        let source = root.join("source.rs");
        std::fs::write(&source, "fn main() {}\n").expect("source");
        let mut app = App::new(&[source.to_string_lossy().into_owned()]).expect("app");
        let current_uri = format!("file://{}", source.display());
        let edit = json!({
            "documentChanges": [{
                "kind": "delete",
                "uri": current_uri
            }]
        });
        assert_eq!(
            app.apply_workspace_edit(&format!("file://{}", source.display()), &edit)
                .expect("delete active resource"),
            1
        );
        assert!(!source.exists());
        assert_eq!(app.buffer().path(), Some(source.as_path()));
        assert!(app.status_message().contains("deleted"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_output_toggles_split_diff_view() {
        let mut app = App::new(&[]).expect("welcome app");
        app.change_diff = vec![DiffLine {
            kind: '+',
            text: "added".into(),
        }];
        app.focus = Focus::Output;
        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("toggle split diff");
        assert!(app.diff_split());
        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::NONE))
            .expect("toggle unified diff");
        assert!(!app.diff_split());
    }

    #[test]
    fn git_output_split_enter_opens_current_change_for_editing() {
        let root = std::env::temp_dir().join(format!("mtide-split-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("file.txt"), "changed").expect("file");
        let mut app = App::new(&[root.to_string_lossy().into()]).expect("app");
        app.changes = vec![GitChange {
            index: ' ',
            worktree: 'M',
            path: "file.txt".into(),
        }];
        app.change_diff = vec![DiffLine {
            kind: '+',
            text: "changed".into(),
        }];
        app.diff_split = true;
        app.focus = Focus::Output;
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open split edit");
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.file_lines(), &["changed".to_owned()]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_changes_open_an_independent_split_diff_tab() {
        let mut app = App::new(&[]).expect("app");
        app.change_diff = vec![
            DiffLine {
                kind: '@',
                text: "@@ -1 +1 @@".into(),
            },
            DiffLine {
                kind: '-',
                text: "-old".into(),
            },
            DiffLine {
                kind: '+',
                text: "+new".into(),
            },
        ];
        app.open_git_diff_tab();
        assert_eq!(
            app.plugin_file_view().map(|(title, _)| title),
            Some("Git diff")
        );
        assert!(app
            .plugin_file_view()
            .is_some_and(|(_, text)| text.contains("│")));
        app.handle_plugin_file_key(key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app
            .plugin_file_view()
            .is_some_and(|(_, text)| !text.contains("│")));
    }

    #[test]
    fn applies_lsp_formatting_text_edits() {
        let mut buffer = Buffer::new(vec!["let x=1;".into()], None);
        let changed = apply_lsp_text_edits(
            &mut buffer,
            "file:///tmp/test.rs",
            &json!([{
                "range":{"start":{"line":0,"character":5},"end":{"line":0,"character":6}},
                "newText":" = "
            }]),
        );
        assert_eq!(changed, 1);
        assert_eq!(buffer.lines(), &["let x = 1;"]);
    }

    #[test]
    fn format_on_save_without_lsp_still_writes_file() {
        let path = std::env::temp_dir().join(format!("mtide-format-save-{}", std::process::id()));
        std::fs::write(&path, "before").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.settings.editor.format_on_save = true;
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit");
        app.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("save");
        assert_eq!(
            std::fs::read_to_string(&path).expect("saved file"),
            "xbefore"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn external_formatter_replaces_buffer_from_stdin_stdout() {
        let path = std::env::temp_dir().join(format!(
            "mtide-external-formatter-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "hello\n").expect("source");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.settings
            .formatters
            .insert("txt".to_owned(), "tr a-z A-Z".to_owned());
        app.run_external_formatter().expect("format");
        assert_eq!(app.file_lines(), &["HELLO"]);
        assert!(app.buffer().dirty());
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn format_on_save_persists_after_async_formatting_response() {
        let path =
            std::env::temp_dir().join(format!("mtide-format-save-async-{}", std::process::id()));
        std::fs::write(&path, "before").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit");
        app.format_on_save_pending = true;
        let uri = path_to_uri(&path);
        app.handle_lsp_edits_result(
            "textDocument/formatting",
            "textDocument/formatting",
            &uri,
            json!([{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}
                },
                "newText": "y"
            }]),
        )
        .expect("format response");
        assert_eq!(
            std::fs::read_to_string(&path).expect("saved file"),
            "ybefore"
        );
        assert!(!app.format_on_save_pending);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn background_poll_reloads_clean_files_but_preserves_dirty_files() {
        let path = std::env::temp_dir().join(format!("mtide-external-app-{}", std::process::id()));
        std::fs::write(&path, "before").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "external").expect("external write");
        app.poll_background();
        assert_eq!(app.file_lines(), &["external"]);

        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "second external").expect("external write");
        app.poll_background();
        assert_eq!(app.file_lines(), &["xexternal"]);
        assert!(app.status_message().contains("Changed on disk"));
        app.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("blocked save");
        assert_eq!(
            std::fs::read_to_string(&path).expect("external file remains"),
            "second external"
        );
        assert!(app.status_message().contains("reload or Save As"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ctrl_alt_r_explicitly_reloads_dirty_file_from_disk() {
        let path = std::env::temp_dir().join(format!("mtide-reload-{}", std::process::id()));
        std::fs::write(&path, "before").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "external").expect("external write");
        app.poll_background();
        assert_eq!(app.file_lines(), &["xbefore"]);
        app.handle_key(key(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("reload");
        assert_eq!(app.file_lines(), &["external"]);
        assert!(!app.buffer().dirty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ctrl_a_selects_entire_document() {
        let path = std::env::temp_dir().join(format!("mtide-select-all-{}", std::process::id()));
        std::fs::write(&path, "one\ntwo").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .expect("select all");
        assert_eq!(app.selection_lines(), Some((0, 1)));
        assert_eq!(
            app.buffer()
                .text_between(Position { line: 0, column: 0 }, app.cursor()),
            "one\ntwo"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn editor_auto_closes_and_skips_pairs() {
        let path = std::env::temp_dir().join(format!("mtide-autopair-{}", std::process::id()));
        std::fs::write(&path, "").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.handle_key(key(KeyCode::Char('('), KeyModifiers::NONE))
            .expect("open pair");
        assert_eq!(app.file_lines(), &["()"]);
        assert_eq!(app.cursor().column, 1);
        app.handle_key(key(KeyCode::Char(')'), KeyModifiers::NONE))
            .expect("skip pair");
        assert_eq!(app.cursor().column, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn editor_outdents_closing_brace_after_indented_newline() {
        let path =
            std::env::temp_dir().join(format!("mtide-auto-indent-close-{}", std::process::id()));
        std::fs::write(&path, "{").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.buffer_mut().move_end();
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("newline");
        app.handle_key(key(KeyCode::Char('}'), KeyModifiers::NONE))
            .expect("closing brace");
        assert_eq!(app.file_lines(), &["{", "}"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn editor_outdents_parenthesis_and_bracket_after_indented_newline() {
        for (index, (opening, closing)) in [('(', ')'), ('[', ']')].into_iter().enumerate() {
            let path = std::env::temp_dir().join(format!(
                "mtide-auto-indent-delimiter-{}-{index}",
                std::process::id()
            ));
            std::fs::write(&path, opening.to_string()).expect("file");
            let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
            app.buffer_mut().move_end();
            app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
                .expect("newline");
            app.handle_key(key(KeyCode::Char(closing), KeyModifiers::NONE))
                .expect("closing delimiter");
            assert_eq!(
                app.file_lines(),
                &[opening.to_string(), closing.to_string()]
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn editor_can_disable_auto_closing_brackets() {
        let path = std::env::temp_dir().join(format!("mtide-no-autopair-{}", std::process::id()));
        std::fs::write(&path, "").expect("file");
        let config = path.with_extension("json");
        std::fs::write(&config, r#"{"editor":{"autoClosingBrackets":false}}"#).expect("config");
        let args = vec![
            "--config".to_owned(),
            config.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ];
        let mut app = App::new(&args).expect("app");
        app.handle_key(key(KeyCode::Char('('), KeyModifiers::NONE))
            .expect("insert");
        assert_eq!(app.file_lines(), &["("]);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(config);
    }

    #[test]
    fn editor_tabs_preserve_each_buffer_and_switch() {
        let root = std::env::temp_dir().join(format!("mtide-tabs-{}", std::process::id()));
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&first, "first").expect("first");
        std::fs::write(&second, "second").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.buffer_mut().insert("!");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        assert_eq!(app.tab_titles().len(), 2);
        assert_eq!(app.file_lines(), &["second"]);
        app.switch_tab(-1);
        assert_eq!(app.file_lines(), &["!first"]);
        assert!(app.tab_titles()[0].contains('•'));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_replace_all_preserves_open_buffers_and_disk_baselines() {
        let root =
            std::env::temp_dir().join(format!("mtide-replace-open-buffers-{}", std::process::id()));
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&first, "one one").expect("first");
        std::fs::write(&second, "one").expect("second");

        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit second");
        app.switch_tab(-1);
        app.search_query = "one".to_owned();
        app.replace_query = "two".to_owned();
        app.refresh_search_results();
        app.replace_workspace_all().expect("replace all");

        assert_eq!(app.file_lines(), &["two two"]);
        app.switch_tab(1);
        assert_eq!(app.file_lines(), &["xtwo"]);
        assert_eq!(
            std::fs::read_to_string(&first).expect("first disk"),
            "one one"
        );
        assert_eq!(
            std::fs::read_to_string(&second).expect("second disk"),
            "one"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reopening_existing_tab_clears_transient_editor_state() {
        let root =
            std::env::temp_dir().join(format!("mtide-reopen-tab-state-{}", std::process::id()));
        let path = root.join("main.rs");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&path, "one\ntwo").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.selection_anchor = Some(Position { line: 0, column: 0 });
        app.secondary_cursors.push(Position { line: 1, column: 1 });
        app.secondary_selections.push((
            Position { line: 0, column: 0 },
            Position { line: 0, column: 1 },
        ));
        app.folded_lines.insert(0);
        app.horizontal_scroll = 4;
        app.open_buffer(Buffer::open(&path).expect("reopen"));
        assert!(app.selection_anchor.is_none());
        assert!(app.secondary_cursors.is_empty());
        assert!(app.secondary_selections.is_empty());
        assert!(app.folded_lines.is_empty());
        assert_eq!(app.horizontal_scroll, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn language_override_is_scoped_to_tab_and_survives_switching() {
        let root = std::env::temp_dir().join(format!("mtide-language-tabs-{}", std::process::id()));
        let first = root.join("first.rs");
        let second = root.join("second.py");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&first, "fn main() {}").expect("first");
        std::fs::write(&second, "print('ok')").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));

        assert_eq!(app.language(), "py");
        app.handle_key(key(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("set language override");
        assert_eq!(app.language(), "js");
        assert_eq!(app.buffer().language_override(), Some("js"));

        app.switch_tab(-1);
        assert_eq!(app.language(), "rs");
        app.handle_key(key(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("set first language override");
        assert_eq!(app.language(), "rust");
        app.handle_key(key(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .expect("advance first language override");
        assert_eq!(app.language(), "go");

        app.switch_tab(1);
        assert_eq!(app.language(), "js");
        app.switch_tab(-1);
        assert_eq!(app.language(), "go");
        app.quit();

        let restored = App::new(&[first.to_string_lossy().into_owned()]).expect("restored app");
        assert_eq!(restored.language(), "go");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_all_on_quit_persists_dirty_tabs() {
        let root = std::env::temp_dir().join(format!("mtide-save-all-{}", std::process::id()));
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(&first, "one").expect("first");
        std::fs::write(&second, "two").expect("second");
        let mut app = App::new(&[first.to_string_lossy().into_owned()]).expect("app");
        app.settings.save_all_on_quit = true;
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit first");
        app.open_buffer(Buffer::open(&second).expect("second buffer"));
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("edit second");
        app.quit();
        assert!(app.should_quit());
        assert_eq!(
            std::fs::read_to_string(&first).expect("first saved"),
            "xone"
        );
        assert_eq!(
            std::fs::read_to_string(&second).expect("second saved"),
            "ytwo"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn autosave_persists_dirty_buffer_after_delay() {
        let root = std::env::temp_dir().join(format!("mtide-autosave-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("directory");
        let path = root.join("note.rs");
        std::fs::write(&path, "one").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.settings.editor.auto_save = true;
        app.settings.editor.auto_save_delay_ms = 500;
        app.last_autosave = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("instant");
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("edit");
        app.poll_background();
        assert_eq!(std::fs::read_to_string(&path).expect("saved"), "xone");
        assert!(!app.buffer().dirty());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn save_all_on_quit_does_not_overwrite_dirty_external_changes() {
        let path =
            std::env::temp_dir().join(format!("mtide-save-all-external-{}", std::process::id()));
        std::fs::write(&path, "before").expect("file");
        let mut app = App::new(&[path.to_string_lossy().into_owned()]).expect("app");
        app.settings.save_all_on_quit = true;
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("local edit");
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "external").expect("external edit");
        app.poll_background();

        app.quit();

        assert!(!app.should_quit());
        assert_eq!(
            std::fs::read_to_string(&path).expect("external file"),
            "external"
        );
        assert!(app.status_message().contains("reload or Save As"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn executable_lookup_checks_path_without_launching_process() {
        assert!(executable_available("sh"));
        assert!(!executable_available("mtide-command-that-does-not-exist"));
    }

    #[test]
    fn agent_session_round_trips_provider_input_and_screen() {
        let path = std::env::temp_dir().join(format!("mtide-agent-session-{}", std::process::id()));
        let session = AgentSession {
            version: AGENT_SESSION_VERSION,
            provider: "local".into(),
            input: "review".into(),
            screen: vec!["ready".into(), "output".into()],
        };
        save_agent_session(&path, &session).expect("save session");
        assert_eq!(load_agent_session(&path), Some(session));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn agent_session_rejects_oversized_or_unknown_state() {
        let path = std::env::temp_dir().join(format!(
            "mtide-agent-session-invalid-{}",
            std::process::id()
        ));
        let oversized = AgentSession {
            version: AGENT_SESSION_VERSION,
            provider: "local".into(),
            input: "x".repeat(AGENT_SESSION_MAX_INPUT_BYTES + 1),
            screen: Vec::new(),
        };
        assert!(save_agent_session(&path, &oversized).is_err());
        std::fs::write(
            &path,
            r#"{"version":99,"provider":"local","input":"","screen":[]}"#,
        )
        .expect("write invalid session");
        assert!(load_agent_session(&path).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn agent_provider_restores_from_session_when_still_configured() {
        let mut settings = Settings::default();
        settings.agent.active = Some("local".to_owned());
        settings.agent.providers.insert(
            "local".to_owned(),
            crate::config::AgentProviderConfig {
                extra: std::collections::BTreeMap::new(),
                name: "Local".to_owned(),
                command: vec!["sh".to_owned()],
                cwd: None,
                install: None,
                tools: Vec::new(),
                persist: false,
            },
        );
        settings.agent.providers.insert(
            "review".to_owned(),
            crate::config::AgentProviderConfig {
                extra: std::collections::BTreeMap::new(),
                name: "Review".to_owned(),
                command: vec!["sh".to_owned()],
                cwd: None,
                install: None,
                tools: Vec::new(),
                persist: false,
            },
        );
        let session = AgentSession {
            version: AGENT_SESSION_VERSION,
            provider: "review".to_owned(),
            input: String::new(),
            screen: Vec::new(),
        };
        assert_eq!(select_agent_provider(&settings, Some(&session)), "review");
        assert_eq!(select_agent_provider(&settings, None), "local");
    }

    #[test]
    fn agent_tool_permissions_are_provider_scoped_and_default_deny() {
        let mut app = App::new(&[]).expect("app");
        app.settings.agent.tools = vec!["read_file".to_owned()];
        app.settings.agent.providers.insert(
            "local".to_owned(),
            crate::config::AgentProviderConfig {
                extra: std::collections::BTreeMap::new(),
                name: "Local".to_owned(),
                command: vec!["sh".to_owned()],
                cwd: None,
                install: None,
                tools: vec!["shell".to_owned()],
                persist: false,
            },
        );
        app.agent_provider = "local".to_owned();
        assert!(app.agent_tool_allowed("shell"));
        assert!(!app.agent_tool_allowed("read_file"));
        assert!(!app.agent_tool_allowed("network"));
        app.settings
            .agent
            .providers
            .get_mut("local")
            .unwrap()
            .tools
            .clear();
        assert!(app.agent_tool_allowed("read_file"));
    }

    #[test]
    fn agent_structured_tools_are_permission_gated_and_workspace_scoped() {
        let root = std::env::temp_dir().join(format!("mtide-agent-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("note.txt"), "hello").expect("file");
        let second =
            root.with_file_name(format!("mtide-agent-tools-second-{}", std::process::id()));
        std::fs::create_dir_all(&second).expect("second root");
        std::fs::write(second.join("other.txt"), "second workspace").expect("second file");
        let mut app = App::new(&[root.to_string_lossy().into()]).expect("app");
        app.open_external_target(&second.to_string_lossy())
            .expect("add second workspace");
        app.settings.agent.tools = vec![
            "read_file".into(),
            "list_files".into(),
            "write_file".into(),
            "search".into(),
        ];
        app.open_buffer(Buffer::open(root.join("note.txt")).expect("open note buffer"));
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("local note edit");
        assert!(app
            .agent_tool_call(
                "write_file",
                &json!({"path": "note.txt", "content": "agent overwrite"})
            )
            .is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).expect("note remains"),
            "hello"
        );
        app.buffer_mut().save().expect("save note buffer");
        let result = app
            .agent_tool_call("read_file", &json!({"path": "note.txt"}))
            .expect("read tool");
        assert_eq!(result["content"], "xhello");
        let second_result = app
            .agent_tool_call("read_file", &json!({"path": "other.txt"}))
            .expect("read second workspace file");
        assert_eq!(second_result["content"], "second workspace");
        let written = app
            .agent_tool_call(
                "write_file",
                &json!({"path": "note.txt", "content": "updated"}),
            )
            .expect("write tool");
        assert_eq!(written["bytes"], 7);
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).expect("updated"),
            "updated"
        );
        app.agent_tool_call(
            "write_file",
            &json!({"path": "created.txt", "content": "created"}),
        )
        .expect("create tool");
        assert_eq!(
            std::fs::read_to_string(root.join("created.txt")).expect("created"),
            "created"
        );
        std::fs::write(root.join("a.txt"), "a").expect("a");
        std::fs::write(root.join("z.txt"), "z").expect("z");
        let listed = app
            .agent_tool_call("list_files", &json!({"path": "."}))
            .expect("list tool");
        assert_eq!(
            listed["entries"].as_array().expect("entries"),
            &vec![
                json!("a.txt"),
                json!("created.txt"),
                json!("note.txt"),
                json!("z.txt")
            ]
        );
        std::fs::write(
            root.join("large.txt"),
            vec![b'x'; AGENT_TOOL_MAX_FILE_BYTES as usize + 1],
        )
        .expect("large file");
        assert!(app
            .agent_tool_call("read_file", &json!({"path": "large.txt"}))
            .is_err());
        assert!(app
            .agent_tool_call(
                "write_file",
                &json!({"path": "large-write.txt", "content": "x".repeat(AGENT_TOOL_MAX_FILE_BYTES as usize + 1)})
            )
            .is_err());
        #[cfg(unix)]
        {
            let outside = root.join("outside.txt");
            let temporary = root.join(format!("note.mtide-agent-{}", std::process::id()));
            std::fs::write(&outside, "protected").expect("outside");
            std::os::unix::fs::symlink(&outside, &temporary).expect("temporary symlink");
            assert!(app
                .agent_tool_call(
                    "write_file",
                    &json!({"path": "note.txt", "content": "must not escape"})
                )
                .is_err());
            assert_eq!(
                std::fs::read_to_string(&outside).expect("protected file"),
                "protected"
            );
            std::fs::remove_file(&temporary).expect("temporary symlink cleanup");
            std::fs::remove_file(&outside).expect("outside cleanup");
        }
        let search = app
            .agent_tool_call(
                "search",
                &json!({"path": ".", "query": "workspace", "limit": 5}),
            )
            .expect("search tool");
        assert!(search["results"]
            .as_array()
            .expect("search results")
            .iter()
            .any(|result| result["path"]
                .as_str()
                .unwrap_or_default()
                .ends_with("other.txt")));
        assert!(app
            .agent_tool_call("shell", &json!({"path": "note.txt"}))
            .is_err());
        assert!(app
            .agent_tool_call("read_file", &json!({"path": "/etc/hosts"}))
            .is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
        std::fs::remove_dir_all(second).expect("second cleanup");
    }

    #[test]
    fn unreadable_startup_file_falls_back_to_welcome_with_error_status() {
        let path = std::env::temp_dir().join(format!("mtide-missing-{}.rs", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let app = App::new(&[path.to_string_lossy().into_owned()]).expect("fallback app");
        assert_eq!(app.file_lines()[0], "# mtide");
        assert!(app.status_message().contains("Open failed"));
    }

    #[test]
    fn terminal_control_bytes_follow_ascii_control_mapping() {
        assert_eq!(terminal_control_byte('a'), Some(1));
        assert_eq!(terminal_control_byte('L'), Some(12));
        assert_eq!(terminal_control_byte('['), Some(0x1b));
        assert_eq!(terminal_control_byte(' '), Some(0));
        assert_eq!(terminal_control_byte('1'), None);
    }

    #[test]
    fn terminal_modified_keys_use_xterm_modifier_sequences() {
        assert_eq!(
            terminal_modified_key(key(KeyCode::Left, KeyModifiers::ALT)),
            Some(b"\x1b[1;3D".to_vec())
        );
        assert_eq!(
            terminal_modified_key(key(KeyCode::PageDown, KeyModifiers::CONTROL)),
            Some(b"\x1b[6;5~".to_vec())
        );
        assert_eq!(
            terminal_modified_key(key(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            terminal_modified_key(key(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            terminal_modified_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            terminal_modified_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn markdown_image_directory_rejects_paths_outside_document_tree() {
        assert_eq!(
            safe_markdown_image_dir(".mtide/assets"),
            Some(PathBuf::from(".mtide/assets"))
        );
        assert_eq!(safe_markdown_image_dir("../shared"), None);
        assert_eq!(safe_markdown_image_dir("/tmp/images"), None);
        assert_eq!(safe_markdown_image_dir(""), None);
    }

    #[test]
    fn markdown_image_bytes_are_written_and_inserted_as_relative_link() {
        let root =
            std::env::temp_dir().join(format!("mtide-markdown-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let document = root.join("notes.md");
        std::fs::write(&document, "# Notes\n").expect("document");
        let mut app = App::new(&[document.to_string_lossy().into_owned()]).expect("app");
        app.settings.editor.markdown_image_dir = ".assets".to_owned();
        assert!(app
            .insert_markdown_image_bytes(b"fake png bytes")
            .expect("insert image"));
        assert!(app
            .file_lines()
            .iter()
            .any(|line| line.contains("![image](.assets/")));
        let assets = root.join(".assets");
        let image = std::fs::read_dir(&assets)
            .expect("assets")
            .next()
            .expect("image entry")
            .expect("image directory entry")
            .path();
        assert_eq!(
            std::fs::read(image).expect("image bytes"),
            b"fake png bytes"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn vt_function_keys_use_standard_terminal_sequences() {
        assert_eq!(vt_function_key(1), b"\x1bOP");
        assert_eq!(vt_function_key(4), b"\x1bOS");
        assert_eq!(vt_function_key(5), b"\x1b[15~");
        assert_eq!(vt_function_key(12), b"\x1b[24~");
        assert_eq!(vt_function_key(13), b"");
    }

    #[test]
    fn invalid_workspace_file_falls_back_with_error_status() {
        let path = std::env::temp_dir().join(format!("mtide-invalid-{}.ttt", std::process::id()));
        std::fs::write(&path, "not json").expect("workspace file");
        let app = App::new(&[path.to_string_lossy().into_owned()]).expect("fallback app");
        assert_eq!(app.file_lines()[0], "# mtide");
        assert!(app.status_message().contains("Workspace open failed"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn visual_wrap_movement_preserves_screen_column_for_wide_text() {
        assert_eq!(wrapped_row_count("abcd", 4), 2);
        assert_eq!(wrapped_row_count("你abc", 4), 2);
        assert_eq!(visual_row_for_column("你abc", 3, 4), 1);
        assert_eq!(column_for_visual_position("你abc", 1, 0, 4), 3);
        assert_eq!(column_for_visual_position("abcdef", 1, 1, 4), 5);
    }

    #[test]
    fn current_word_at_cursor_is_unicode_safe() {
        assert_eq!(
            current_word_at_cursor("let 变量_name = true", 6),
            "变量_name"
        );
        assert_eq!(current_word_at_cursor("alpha beta", 5), "alpha");
        assert_eq!(current_word_at_cursor("alpha beta", 6), "beta");
        assert_eq!(current_word_at_cursor("alpha beta", 10), "beta");
        assert_eq!(current_word_at_cursor("alpha - beta", 7), "");
    }

    #[test]
    fn editable_diff_markers_follow_current_line_order() {
        let base = vec!["same".into(), "old".into(), "tail".into()];
        let current = vec!["same".into(), "new".into(), "tail".into(), "added".into()];
        assert_eq!(
            editor_diff_markers(&base, &current),
            vec![None, Some('+'), None, Some('+')]
        );
    }
}
