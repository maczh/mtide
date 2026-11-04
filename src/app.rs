use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::{
    fs,
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
};

use crate::{
    config::{Settings, Theme},
    core::{Buffer, ExternalChange, Position},
    error::AppResult,
    git::{
        apply_hunk as git_apply_hunk, base_file as git_base_file, blame as git_blame,
        branches as git_branches, checkout as git_checkout, checkout_conflict_side,
        commit as git_commit, conflict_blocks, create_branch as git_create_branch,
        current_branch as git_branch, delete_branch as git_delete_branch, diff as git_diff,
        diff_refs as git_diff_refs, fetch as git_fetch,
        fetch_pull_request as git_fetch_pull_request, history as git_history,
        hunk_count as git_hunk_count, hunk_start_line,
        list_pull_request_comments as git_list_pr_comments, pull as git_pull, push as git_push,
        remote_with_credentials as git_remote_credentials, resolve_ref as git_resolve_ref,
        stage as git_stage, status as git_status, unstage as git_unstage, BlameLine, DiffLine,
        GitChange, GitCommit, PullRequestComment,
    },
    lsp::{LspManager, LspServerState},
    plugin::{
        download_plugin_registry_entry, fetch_plugin_registry, install_plugin_archive,
        PluginEditorRequest, PluginEditorSnapshot, PluginEventArg, PluginPackage,
        PluginPanelLocation, PluginPanelRequest, PluginRegistryEntry, PluginRuntime,
    },
    search::{
        replace_with_options, search_text_with_options, search_workspace_roots_with_options,
        SearchResult,
    },
    terminal::{TerminalState, TerminalStyle},
};
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCredentialPrompt {
    pub operation: String,
    pub username: String,
    pub password: String,
    pub password_focus: bool,
}

#[derive(Debug)]
struct GitRemoteCompletion {
    operation: String,
    result: Result<(), String>,
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
        items: Vec<String>,
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
    agent_visible: bool,
    quit: bool,
    buffers: Vec<Buffer>,
    active_tab: usize,
    search_query: String,
    replace_query: String,
    replace_mode: bool,
    search_regex: bool,
    search_results: Vec<SearchResult>,
    search_index: usize,
    search_root: PathBuf,
    workspace_roots: Vec<PathBuf>,
    explorer_roots: Vec<PathBuf>,
    explorer_paths: Vec<PathBuf>,
    explorer_expanded: BTreeSet<PathBuf>,
    explorer_index: usize,
    explorer_mode: Option<ExplorerMode>,
    explorer_input: String,
    changes: Vec<GitChange>,
    branch: Option<String>,
    branches: Vec<String>,
    branch_index: usize,
    branch_mode: bool,
    branch_name: String,
    commit_mode: bool,
    commit_message: String,
    change_index: usize,
    change_hunk_index: usize,
    change_diff: Vec<DiffLine>,
    diff_split: bool,
    pull_request_target: Option<(String, u64)>,
    pull_request_head: Option<String>,
    pull_request_comments: Vec<PullRequestComment>,
    pull_request_comment_mode: bool,
    blame_lines: Vec<BlameLine>,
    history: Vec<GitCommit>,
    output_scroll: usize,
    settings: Settings,
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
    git_credential_prompt: Option<GitCredentialPrompt>,
    git_remote_rx: Receiver<GitRemoteCompletion>,
    git_remote_tx: Sender<GitRemoteCompletion>,
    git_remote_pending: Option<String>,
    lsp: LspManager,
    lsp_popup: Option<LspPopup>,
    rename_mode: bool,
    rename_text: String,
    plugin_palette: bool,
    plugin_query: String,
    plugin_index: usize,
    settings_overlay: bool,
    settings_index: usize,
    overlay_focus: Option<Focus>,
    quick_open: bool,
    quick_open_query: String,
    quick_open_save_as: bool,
    goto_mode: bool,
    goto_input: String,
    selection_anchor: Option<Position>,
    clipboard: String,
    osc52_clipboard: Option<String>,
    horizontal_scroll: usize,
    editor_scroll_line: Option<usize>,
    secondary_cursors: Vec<Position>,
    secondary_selections: Vec<(Position, Position)>,
    column_selection: Option<(Position, Position)>,
    mouse_selecting: bool,
    folded_lines: BTreeSet<usize>,
    diff_base_lines: Option<Vec<String>>,
    plugin_mouse_events: Vec<MouseEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginDrawerState {
    runtime_index: usize,
    side: String,
    width: u16,
    min_width: u16,
}
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
            if let Some(path) = settings_path {
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
        let (agent_command, agent_cwd) = settings.agent.providers.get(&agent_provider).map_or_else(
            || (settings.agent.command.clone(), settings.agent.cwd.clone()),
            |provider| (provider.command.clone(), provider.cwd.clone()),
        );
        let agent =
            TerminalState::with_command(agent_command, agent_cwd.as_ref().map(PathBuf::from));
        let mut agent = agent;
        if let Some(session) = saved_agent_session
            .as_ref()
            .filter(|session| session.provider == agent_provider)
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
        settings.apply_editorconfig(&search_root, &language);
        let plugins: Vec<_> = args
            .windows(2)
            .filter(|pair| pair[0] == "--plugin")
            .map(|pair| PluginPackage::load(&pair[1]))
            .collect::<AppResult<Vec<_>>>()?;
        let plugin_runtimes: Vec<_> = plugins
            .iter()
            .filter(|package| package.manifest.entry.is_some())
            .map(PluginRuntime::load)
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
            if let Ok(theme) = Theme::from_path(path) {
                theme_options.push((path.clone(), theme));
            }
        }
        for path in plugins.iter().flat_map(PluginPackage::theme_paths) {
            if let Ok(theme) = Theme::from_path(&path) {
                theme_options.push((path.display().to_string(), theme));
            }
        }
        let theme_index = if theme_options.len() > 1 { 1 } else { 0 };
        let theme = theme_options[theme_index].1;
        let changes = git_status(&search_root);
        let branch = git_branch(&search_root);
        let branches = git_branches(&search_root);
        let change_diff = changes
            .first()
            .map_or_else(Vec::new, |change| git_diff(&search_root, change));
        let explorer_paths = explorer_roots
            .iter()
            .flat_map(|root| list_directory(root))
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
        let mut app = Self {
            focus: Focus::Editor,
            sidebar_tab: SidebarTab::Explorer,
            bottom_tab: BottomTab::Terminal,
            sidebar_visible: settings.sidebar_visible,
            bottom_visible: settings.bottom_visible,
            agent_visible: settings.agent_visible,
            quit: false,
            buffers: vec![buffer],
            active_tab: 0,
            search_query: String::new(),
            replace_query: String::new(),
            replace_mode: false,
            search_regex: false,
            search_results: Vec::new(),
            search_index: 0,
            search_root,
            workspace_roots,
            explorer_roots,
            explorer_paths,
            explorer_expanded: BTreeSet::new(),
            explorer_index: 0,
            explorer_mode: None,
            explorer_input: String::new(),
            changes,
            branch,
            branches,
            branch_index: 0,
            branch_mode: false,
            branch_name: String::new(),
            commit_mode: false,
            commit_message: String::new(),
            change_index: 0,
            change_hunk_index: 0,
            change_diff,
            diff_split: false,
            pull_request_target: None,
            pull_request_head: None,
            pull_request_comments: Vec::new(),
            pull_request_comment_mode: false,
            blame_lines: Vec::new(),
            history: Vec::new(),
            output_scroll: 0,
            settings,
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
            git_credential_prompt: None,
            git_remote_rx,
            git_remote_tx,
            git_remote_pending: None,
            lsp,
            lsp_popup: None,
            rename_mode: false,
            rename_text: String::new(),
            plugin_palette: false,
            plugin_query: String::new(),
            plugin_index: 0,
            settings_overlay: false,
            settings_index: 0,
            overlay_focus: None,
            quick_open: false,
            quick_open_query: String::new(),
            quick_open_save_as: false,
            goto_mode: false,
            goto_input: String::new(),
            selection_anchor: None,
            clipboard: String::new(),
            osc52_clipboard: None,
            horizontal_scroll: 0,
            editor_scroll_line: None,
            secondary_cursors: Vec::new(),
            secondary_selections: Vec::new(),
            column_selection: None,
            mouse_selecting: false,
            folded_lines: BTreeSet::new(),
            diff_base_lines: None,
            plugin_mouse_events: Vec::new(),
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
    pub const fn agent_visible(&self) -> bool {
        self.agent_visible
    }
    pub const fn sidebar_width(&self) -> u16 {
        self.settings.sidebar_width
    }
    pub const fn agent_width(&self) -> u16 {
        self.settings.agent_width
    }
    pub fn diagnostic_severity(&self, line: usize) -> Option<u8> {
        let path = self.buffer().path()?;
        let path_text = path.to_string_lossy().into_owned();
        let uri = format!("file://{path_text}");
        let lsp_severity = self
            .lsp
            .diagnostics(&uri)
            .iter()
            .filter(|diagnostic| {
                (diagnostic.start.line as usize..=diagnostic.end.line as usize).contains(&line)
            })
            .filter_map(|diagnostic| diagnostic.severity)
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
        let uri = format!("file://{path_text}");
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
    pub fn bookmark_marker(&self, line: usize) -> Option<String> {
        let path = self.buffer().path()?.to_string_lossy().into_owned();
        self.plugin_runtimes
            .iter()
            .find_map(|runtime| runtime.bookmark_marker(&path, line))
    }
    pub fn file_lines(&self) -> &[String] {
        self.buffer().lines()
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
        self.file_lines().join("\n")
    }
    pub fn search_query(&self) -> &str {
        &self.search_query
    }
    pub fn replace_query(&self) -> &str {
        &self.replace_query
    }
    pub const fn replace_mode(&self) -> bool {
        self.replace_mode
    }
    pub fn search_results(&self) -> &[SearchResult] {
        &self.search_results
    }
    pub const fn search_index(&self) -> usize {
        self.search_index
    }
    pub fn changes(&self) -> &[GitChange] {
        &self.changes
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
    pub const fn change_index(&self) -> usize {
        self.change_index
    }
    pub const fn change_hunk_index(&self) -> usize {
        self.change_hunk_index
    }
    pub fn change_hunk_count(&self) -> usize {
        git_hunk_count(&self.change_diff)
    }
    #[allow(dead_code)]
    pub fn load_pull_request_review(&mut self, repository: &str, number: u64) -> AppResult<()> {
        let reference = git_fetch_pull_request(&self.search_root, repository, number)?;
        let diff = git_diff_refs(&self.search_root, "HEAD", &reference);
        if diff.is_empty() {
            return Err(crate::error::AppError::Git(
                "pull request has no reviewable diff".to_owned(),
            ));
        }
        self.change_diff = diff;
        self.change_hunk_index = 0;
        self.diff_split = false;
        self.pull_request_target = Some((repository.to_owned(), number));
        self.pull_request_head = Some(reference);
        self.pull_request_comments.clear();
        self.pull_request_comment_mode = false;
        self.bottom_tab = BottomTab::Output;
        self.status_message = format!("Loaded pull request #{number} review diff");
        Ok(())
    }
    pub fn load_pull_request_comments(&mut self) -> AppResult<()> {
        let Some((repository, number)) = self.pull_request_target.clone() else {
            return Err(crate::error::AppError::Git(
                "no pull request review is active".to_owned(),
            ));
        };
        self.pull_request_comments = git_list_pr_comments(&repository, number)?;
        self.output_scroll = 0;
        self.status_message = format!(
            "Loaded {} pull request review comments",
            self.pull_request_comments.len()
        );
        Ok(())
    }
    fn submit_pull_request_comment(&mut self) -> AppResult<()> {
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
        let commit_id = git_resolve_ref(&self.search_root, &reference)?;
        let comment = crate::git::create_pull_request_comment(
            &repository,
            number,
            &commit_id,
            &path,
            line,
            &self.commit_message,
        )?;
        self.pull_request_comments.push(comment);
        self.commit_message.clear();
        self.pull_request_comment_mode = false;
        self.status_message = "Pull request comment submitted".to_owned();
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
    pub fn change_diff(&self) -> &[DiffLine] {
        &self.change_diff
    }
    pub const fn diff_split(&self) -> bool {
        self.diff_split
    }
    pub fn pull_request_comments(&self) -> &[PullRequestComment] {
        &self.pull_request_comments
    }
    pub const fn pull_request_comment_mode(&self) -> bool {
        self.pull_request_comment_mode
    }
    pub const fn output_scroll(&self) -> usize {
        self.output_scroll
    }
    fn output_max_scroll(&self) -> usize {
        self.change_diff
            .len()
            .saturating_sub(1)
            .max(self.history.len().saturating_sub(1))
            .max(self.blame_lines.len().saturating_sub(1))
            .max(self.plugin_registry_entries.len().saturating_sub(1))
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
            _ => return,
        };
        self.select_sidebar(tab);
    }
    pub fn menu_click(&mut self, index: usize) {
        let label = ["File", "Edit", "View", "Options", "Help"]
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
    pub fn terminal_screen_styled_lines(&self) -> Vec<Vec<(char, TerminalStyle)>> {
        self.terminals[self.active_terminal].screen_styled_lines()
    }
    pub fn scroll_terminal(&mut self, delta: isize) {
        self.terminal_mut().scroll(delta);
    }

    pub fn terminal_mouse_tracking(&self) -> bool {
        self.terminals[self.active_terminal].mouse_tracking()
    }

    pub fn terminal_bracketed_paste(&self) -> bool {
        self.terminals[self.active_terminal].bracketed_paste()
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
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            self.status_message = "Blocked terminal link: only http(s) URLs are allowed".to_owned();
            return;
        }
        let result = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(url).spawn()
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .spawn()
        } else {
            std::process::Command::new("xdg-open").arg(url).spawn()
        };
        self.status_message = match result {
            Ok(_) => format!("Opening {url}"),
            Err(error) => format!("Unable to open link: {error}"),
        };
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
        let Some(provider) = self.settings.agent.providers.get(&self.agent_provider) else {
            return self
                .settings
                .agent
                .tools
                .iter()
                .any(|allowed| allowed == tool);
        };
        let permissions = if provider.tools.is_empty() {
            &self.settings.agent.tools
        } else {
            &provider.tools
        };
        permissions.iter().any(|allowed| allowed == tool)
    }

    #[allow(dead_code)]
    pub fn agent_tool_call(&self, tool: &str, args: &JsonValue) -> AppResult<JsonValue> {
        if !self.agent_tool_allowed(tool) {
            return Err(crate::error::AppError::Workspace(format!(
                "agent tool denied: {tool}"
            )));
        }
        let path = args
            .get("path")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                crate::error::AppError::Workspace("agent tool path is required".into())
            })?;
        let path = if tool == "write_file" {
            self.agent_workspace_write_path(path)?
        } else {
            self.agent_workspace_path(path)?
        };
        match tool {
            "read_file" => {
                let contents = fs::read_to_string(&path)?;
                Ok(json!({"path": path.display().to_string(), "content": contents}))
            }
            "list_files" => {
                let entries = fs::read_dir(&path)?
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                Ok(json!({"path": path.display().to_string(), "entries": entries}))
            }
            "write_file" => {
                let content = args
                    .get("content")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| {
                        crate::error::AppError::Workspace(
                            "agent write_file content is required".into(),
                        )
                    })?;
                let temporary = path.with_extension(format!("mtide-agent-{}", std::process::id()));
                fs::write(&temporary, content)?;
                if let Err(error) = fs::rename(&temporary, &path) {
                    let _ = fs::remove_file(&temporary);
                    return Err(error.into());
                }
                Ok(json!({"path": path.display().to_string(), "bytes": content.len()}))
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
                collect_agent_search_results(&path, query, limit, &mut results);
                Ok(json!({"query": query, "results": results}))
            }
            _ => Err(crate::error::AppError::Workspace(format!(
                "unknown agent tool: {tool}"
            ))),
        }
    }

    fn agent_workspace_path(&self, path: &str) -> AppResult<PathBuf> {
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.search_root.join(path)
        };
        let canonical = candidate.canonicalize()?;
        if self
            .workspace_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| canonical.starts_with(root))
        {
            Ok(canonical)
        } else {
            Err(crate::error::AppError::Workspace(
                "agent tool path is outside workspace".into(),
            ))
        }
    }

    fn agent_workspace_write_path(&self, path: &str) -> AppResult<PathBuf> {
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.search_root.join(path)
        };
        if candidate.exists() {
            let canonical = candidate.canonicalize()?;
            if !canonical.is_file() {
                return Err(crate::error::AppError::Workspace(
                    "agent write_file target is not a file".into(),
                ));
            }
            return self.agent_workspace_path(path);
        }
        let parent = candidate.parent().ok_or_else(|| {
            crate::error::AppError::Workspace("agent write_file parent is unavailable".into())
        })?;
        let parent = self.agent_workspace_path(&parent.to_string_lossy())?;
        let name = candidate.file_name().ok_or_else(|| {
            crate::error::AppError::Workspace("agent write_file name is unavailable".into())
        })?;
        Ok(parent.join(name))
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
    pub fn poll_background(&mut self) {
        while let Ok(completion) = self.git_remote_rx.try_recv() {
            self.git_remote_pending = None;
            match completion.result {
                Ok(()) => {
                    self.refresh_git_state();
                    self.status_message = format!("Git {} completed", completion.operation);
                }
                Err(error) => self.handle_git_remote_failure(&completion.operation, &error),
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
        }
        self.agent.poll();
        self.poll_agent_protocol();
        if self.agent_install_pending && self.agent_executable_available() {
            self.agent_install_pending = false;
            self.restart_agent_session();
            self.status_message =
                format!("Agent {} installed · session restarted", self.agent_name());
        }
        self.lsp.poll();
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

    fn poll_agent_protocol(&mut self) {
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
            let Some(tool) = message.get("tool").and_then(JsonValue::as_str) else {
                continue;
            };
            let args = message.get("args").cloned().unwrap_or_else(|| json!({}));
            let id = message.get("id").cloned().unwrap_or(JsonValue::Null);
            let response = match self.agent_tool_call(tool, &args) {
                Ok(result) => json!({"type":"tool_result", "id":id, "ok":true, "result":result}),
                Err(error) => {
                    json!({"type":"tool_result", "id":id, "ok":false, "error":error.to_string()})
                }
            };
            if let Ok(mut encoded) = serde_json::to_vec(&response) {
                encoded.push(b'\n');
                if let Err(error) = self.agent.write_bytes(&encoded) {
                    self.status_message = format!("Agent tool response failed: {error}");
                }
            }
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
                PluginPanelRequest::Show { location, focus } => match location {
                    PluginPanelLocation::Sidebar => {
                        self.sidebar_visible = true;
                        if focus {
                            self.focus = Focus::Explorer;
                        }
                    }
                    PluginPanelLocation::Bottom => {
                        self.bottom_visible = true;
                        self.bottom_tab = BottomTab::Output;
                        if focus {
                            self.focus = Focus::Output;
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
                            self.plugin_custom_tab = Some((runtime_index, id));
                            self.plugin_tab_scroll = 0;
                            self.plugin_tab_hunk = 0;
                            self.plugin_tab_split = false;
                            self.plugin_tab_collapsed_hunks.clear();
                            if focus {
                                self.focus = Focus::Editor;
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
                    if self
                        .plugin_custom_tab
                        .as_ref()
                        .is_some_and(|(owner, current)| *owner == runtime_index && current == &id)
                    {
                        if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                            if let Err(error) = runtime.close_tab(&id) {
                                self.status_message = format!("Plugin tab close failed: {error}");
                            }
                        }
                        self.plugin_custom_tab = None;
                        self.plugin_file_view = None;
                        self.plugin_file_view_path = None;
                        self.plugin_tab_scroll = 0;
                        self.plugin_tab_hunk = 0;
                        self.plugin_tab_split = false;
                        self.plugin_tab_collapsed_hunks.clear();
                        self.focus = Focus::Editor;
                    }
                }
                PluginPanelRequest::FocusTab { id } => {
                    if self
                        .plugin_custom_tab
                        .as_ref()
                        .is_some_and(|(owner, current)| *owner == runtime_index && current == &id)
                    {
                        self.focus = Focus::Editor;
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
                    if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                        if let Err(error) = runtime.command_line_change(&text) {
                            self.status_message = format!("Plugin command line failed: {error}");
                        }
                    }
                }
            }
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
                let text = self
                    .plugin_command_line
                    .as_mut()
                    .map(|command_line| {
                        command_line.text.pop();
                        command_line.text.clone()
                    })
                    .unwrap_or_default();
                if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                    runtime.command_line_change(&text)?;
                }
            }
            KeyCode::Char(value) if key.modifiers.is_empty() => {
                let text = self
                    .plugin_command_line
                    .as_mut()
                    .map(|command_line| {
                        command_line.text.push(value);
                        command_line.text.clone()
                    })
                    .unwrap_or_default();
                if let Some(runtime) = self.plugin_runtimes.get(runtime_index) {
                    runtime.command_line_change(&text)?;
                }
            }
            _ => {}
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
            .join("\n");
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
    pub fn plugin_status_items_by_side(&self) -> (Vec<String>, Vec<String>) {
        let mut items = self
            .plugin_runtimes
            .iter()
            .flat_map(|runtime| runtime.registration().status_items)
            .collect::<Vec<_>>();
        items.sort_by_key(|item| item.priority);
        let (mut left, mut right) = (Vec::new(), Vec::new());
        for item in items {
            if item.side == "right" {
                right.push(item.text);
            } else {
                left.push(item.text);
            }
        }
        (left, right)
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
    pub fn plugin_panel_text(&self) -> Option<String> {
        self.plugin_panel_text_at(PluginPanelLocation::Sidebar)
    }
    pub fn plugin_bottom_panel_text(&self) -> Option<String> {
        self.plugin_panel_text_at(PluginPanelLocation::Bottom)
    }
    pub fn plugin_drawer(&self) -> Option<(&str, u16, u16, String)> {
        let drawer = self.plugin_drawer.as_ref()?;
        let runtime = self.plugin_runtimes.get(drawer.runtime_index)?;
        let text = runtime.render_panel("__drawer").ok().flatten()?;
        Some((drawer.side.as_str(), drawer.width, drawer.min_width, text))
    }
    fn plugin_panel_text_at(&self, location: PluginPanelLocation) -> Option<String> {
        self.plugin_runtimes.iter().find_map(|runtime| {
            let panel_id = runtime
                .registration()
                .panels
                .into_iter()
                .find(|panel| panel.location == location)?
                .id;
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
    pub const fn plugin_file_view_scroll(&self) -> usize {
        self.plugin_tab_scroll
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
        ]
    }
    pub const fn quick_open_open(&self) -> bool {
        self.quick_open
    }
    pub fn quick_open_query(&self) -> &str {
        &self.quick_open_query
    }
    pub fn quick_open_title(&self) -> &'static str {
        if self.quick_open_save_as {
            "Save As"
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
        let query = self.plugin_query.to_lowercase();
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

    fn buffer(&self) -> &Buffer {
        &self.buffers[self.active_tab]
    }

    fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.active_tab]
    }

    fn terminal_mut(&mut self) -> &mut TerminalState {
        &mut self.terminals[self.active_terminal]
    }

    fn write_terminal_cursor_key(&mut self, normal: &[u8], application: &[u8]) -> AppResult<()> {
        let use_application = self.terminals[self.active_terminal].application_cursor_keys();
        self.terminal_mut()
            .write_bytes(if use_application { application } else { normal })
    }

    fn open_terminal(&mut self) {
        let command = terminal_command(&self.settings);
        let mut terminal = TerminalState::with_command(command, None);
        terminal.set_scrollback(self.settings.terminal.scrollback);
        self.terminals.push(terminal);
        self.active_terminal = self.terminals.len() - 1;
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
    }

    pub fn mouse_column_down(&mut self, position: Position) {
        self.selection_anchor = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.column_selection = Some((position, position));
        self.buffer_mut().set_cursor(position.line, position.column);
        self.mouse_selecting = true;
        self.focus = Focus::Editor;
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
    }
    pub fn mouse_drag(&mut self, position: Position) {
        if self.mouse_selecting {
            self.buffer_mut().set_cursor(position.line, position.column);
        }
    }

    pub fn mouse_column_drag(&mut self, position: Position) {
        if let Some((anchor, _)) = self.column_selection {
            self.column_selection = Some((anchor, position));
            self.buffer_mut().set_cursor(position.line, position.column);
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
        self.diff_base_lines = None;
        self.folded_lines.clear();
        if let Some(path) = buffer.path() {
            if let Some(index) = self
                .buffers
                .iter()
                .position(|candidate| candidate.path() == Some(path))
            {
                self.active_tab = index;
                self.editor_scroll_line = None;
                self.emit_plugin_event("tab.activate");
                return;
            }
        }
        self.buffers.push(buffer);
        self.active_tab = self.buffers.len() - 1;
        self.selection_anchor = None;
        self.editor_scroll_line = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.emit_plugin_event("tab.open");
        self.emit_plugin_event("tab.activate");
    }

    pub fn open_external_target(&mut self, target: &str) -> AppResult<()> {
        let target = parse_location_target(target);
        if target.path.is_dir() {
            self.status_message = format!("Workspace target: {}", target.path.display());
            return Ok(());
        }
        let mut buffer = Buffer::open(&target.path)?;
        buffer.set_cursor(target.line, target.column);
        self.open_buffer(buffer);
        self.focus = Focus::Editor;
        self.sync_lsp_for_buffer();
        self.emit_plugin_event("document.open");
        self.status_message = format!("Opened {}", target.path.display());
        Ok(())
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
        if let Some(path) = self.buffer().path().map(Path::to_path_buf) {
            let language = self.language().to_owned();
            self.notify_lsp_closed(&language, &path);
        }
        self.emit_plugin_event("tab.close");
        self.buffers.remove(self.active_tab);
        self.active_tab = self.active_tab.min(self.buffers.len() - 1);
        self.selection_anchor = None;
        self.editor_scroll_line = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.sync_lsp_for_buffer();
    }

    fn switch_tab(&mut self, delta: isize) {
        if self.buffers.len() < 2 {
            return;
        }
        self.active_tab =
            (self.active_tab as isize + delta).rem_euclid(self.buffers.len() as isize) as usize;
        self.selection_anchor = None;
        self.horizontal_scroll = 0;
        self.editor_scroll_line = None;
        self.secondary_cursors.clear();
        self.secondary_selections.clear();
        self.folded_lines.clear();
        self.diff_base_lines = None;
        self.sync_lsp_for_buffer();
        self.emit_plugin_event("tab.activate");
        self.status_message = format!("Tab {}/{}", self.active_tab + 1, self.buffers.len());
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.git_credential_prompt.is_some() {
            self.handle_git_credential_key(key);
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
                            runtime.dialog_activate(id)?;
                        }
                        KeyCode::Up | KeyCode::Left => {
                            runtime.dialog_dropdown_move(id, -1)?;
                        }
                        KeyCode::Down | KeyCode::Right => {
                            runtime.dialog_dropdown_move(id, 1)?;
                        }
                        KeyCode::Backspace => {
                            runtime.dialog_backspace(id)?;
                        }
                        KeyCode::Char(value) if key.modifiers.is_empty() => {
                            runtime.dialog_append(id, value)?;
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
        if self.dispatch_plugin_keybinding(key)? {
            return Ok(());
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
                        prompt.password.pop();
                    } else {
                        prompt.username.pop();
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                if let Some(prompt) = self.git_credential_prompt.as_mut() {
                    if prompt.password_focus {
                        prompt.password.push(character);
                    } else {
                        prompt.username.push(character);
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
                let root = self.search_root.clone();
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
        self.quick_open_query.clear();
        self.status_message = "Open file · Enter open · Esc cancel".to_owned();
    }
    fn open_save_as(&mut self) {
        self.begin_overlay();
        self.quick_open = true;
        self.quick_open_save_as = true;
        self.quick_open_query = self.buffer().file_name().to_owned();
        self.status_message = "Save As · Enter save · Esc cancel".to_owned();
    }
    fn open_goto(&mut self) {
        self.begin_overlay();
        self.goto_mode = true;
        self.goto_input.clear();
        self.status_message = "Go to line[:column] · Enter jump · Esc cancel".to_owned();
    }
    fn close_goto(&mut self) {
        self.goto_mode = false;
        self.goto_input.clear();
        self.restore_overlay_focus();
    }
    fn handle_goto_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character))
                if character.is_ascii_digit() || character == ':' =>
            {
                self.goto_input.push(character);
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.goto_input.pop();
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
        self.restore_overlay_focus();
    }
    fn handle_quick_open_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => {
                self.quick_open_query.push(character);
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.quick_open_query.pop();
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.close_quick_open();
                self.status_message = "Open file cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let raw = self.quick_open_query.trim();
                if raw.is_empty() {
                    self.status_message = "Enter a file path".to_owned();
                    return Ok(());
                }
                let path = PathBuf::from(raw);
                let path = if path.is_absolute() {
                    path
                } else {
                    self.search_root.join(path)
                };
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
            _ => {}
        }
        Ok(())
    }
    pub fn handle_paste(&mut self, text: &str) -> AppResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.plugin_palette {
            self.plugin_query.push_str(text);
            self.plugin_index = 0;
            return Ok(());
        }
        match self.focus {
            Focus::Search => {
                if self.replace_mode {
                    self.replace_query.push_str(text);
                } else {
                    self.search_query.push_str(text);
                }
                self.refresh_search();
            }
            Focus::Editor => {
                if !self.secondary_selections.is_empty() {
                    self.replace_active_selections(text);
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
            3 => self.settings.editor.word_wrap = !self.settings.editor.word_wrap,
            4 => self.settings.editor.format_on_save = !self.settings.editor.format_on_save,
            5 => self.settings.save_all_on_quit = !self.settings.save_all_on_quit,
            6 => self.settings.terminal.ctrl_v = !self.settings.terminal.ctrl_v,
            7 => self.settings.lsp.enabled = !self.settings.lsp.enabled,
            _ => return,
        }
        self.status_message = "Setting changed for this session".to_owned();
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
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn execute_builtin_command(&mut self, id: &str) -> AppResult<bool> {
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
        let key = match id {
            "editor.save" => KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            "editor.undo" => KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            "editor.redo" => KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            "editor.find" => KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            "editor.gotoLine" => KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
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
            "editor.deleteWordBackward" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
            "editor.deleteWordForward" => KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL),
            "editor.toggleComment" => KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
            "view.toggleSidebar" => KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            "view.toggleTerminal" => KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            "view.toggleAgent" => KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            "view.switchTheme" => KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            "workspace.openFile" => KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
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
        let buffer = self.buffer();
        let cursor = buffer.cursor();
        let selection = self
            .selection_anchor
            .map(|anchor| (anchor.line, anchor.column, cursor.line, cursor.column));
        let selection_text = self
            .selection_anchor
            .map(|anchor| buffer.text_between(anchor, cursor))
            .unwrap_or_default();
        let snapshot = PluginEditorSnapshot {
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
        };
        let path = snapshot.path.clone();
        let payload = match event {
            "document.open" | "document.reload" | "document.save" => {
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
            ],
            _ => Vec::new(),
        };
        let event_names: Vec<&str> = match event {
            "document.open" => vec!["document.open", "file.open", "tab.change"],
            "document.reload" => vec!["document.reload", "file.open"],
            "document.save" => vec!["document.save", "file.save"],
            "tab.activate" => vec!["tab.activate", "tab.change"],
            _ => vec![event],
        };
        for runtime in &self.plugin_runtimes {
            runtime.set_editor_snapshot(snapshot.clone());
            for event_name in &event_names {
                if let Err(error) = runtime.emit_with_payload(event_name, &payload) {
                    self.status_message = format!("Plugin event failed: {error}");
                }
            }
        }
        self.apply_plugin_editor_requests();
        self.apply_plugin_bookmark_events();
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
                "click" => match automation_mouse_coordinates(&task.args, 2) {
                    Ok([x, y, _, _]) => {
                        self.plugin_mouse_events.push(MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column: x,
                            row: y,
                            modifiers: KeyModifiers::NONE,
                        });
                        self.plugin_mouse_events.push(MouseEvent {
                            kind: MouseEventKind::Up(MouseButton::Left),
                            column: x,
                            row: y,
                            modifiers: KeyModifiers::NONE,
                        });
                        result = json!(true);
                    }
                    Err(message) => error = Some(message),
                },
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
    }

    fn apply_plugin_editor_requests(&mut self) {
        let requests = self
            .plugin_runtimes
            .iter()
            .flat_map(PluginRuntime::drain_editor_requests)
            .collect::<Vec<_>>();
        for request in requests {
            let line_count = self.file_lines().len();
            let clamp_position = |line: usize, col: usize, lines: &[String]| {
                let line = line.min(lines.len().saturating_sub(1));
                let col = col.min(lines.get(line).map_or(0, |text| text.chars().count()));
                Position { line, column: col }
            };
            match request {
                PluginEditorRequest::BeginUndoGroup => self.buffer_mut().begin_undo_group(),
                PluginEditorRequest::EndUndoGroup => self.buffer_mut().end_undo_group(),
                PluginEditorRequest::Insert { line, col, text } => {
                    let position = clamp_position(line, col, self.file_lines());
                    self.buffer_mut().set_cursor(position.line, position.column);
                    self.buffer_mut().insert(&text);
                }
                PluginEditorRequest::SetLine { line, text } => {
                    if line < line_count {
                        let end = self.file_lines()[line].chars().count();
                        self.buffer_mut().replace_ranges(
                            &[(Position { line, column: 0 }, Position { line, column: end })],
                            &text,
                        );
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
                }
                PluginEditorRequest::SetCursor { line, col } => {
                    let position = clamp_position(line, col, self.file_lines());
                    self.buffer_mut().set_cursor(position.line, position.column);
                    self.selection_anchor = None;
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
                }
                PluginEditorRequest::ClearSelection => {
                    self.selection_anchor = None;
                    self.secondary_selections.clear();
                    self.column_selection = None;
                }
                PluginEditorRequest::AddCursor { line, col } => {
                    let position = clamp_position(line, col, self.file_lines());
                    if position != self.cursor() && !self.secondary_cursors.contains(&position) {
                        self.secondary_cursors.push(position);
                        self.secondary_cursors
                            .sort_by_key(|cursor| (cursor.line, cursor.column));
                    }
                }
                PluginEditorRequest::ClearCursors => {
                    self.secondary_cursors.clear();
                    self.secondary_selections.clear();
                    self.column_selection = None;
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
            }
        }
    }
    fn handle_changes_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.commit_mode {
            return self.handle_commit_key(key);
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Down) => {
                if !self.changes.is_empty() {
                    self.change_index = (self.change_index + 1).min(self.changes.len() - 1);
                    self.refresh_change_diff();
                }
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.change_index = self.change_index.saturating_sub(1);
                self.refresh_change_diff();
            }
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
                    let path = self.search_root.join(&change.path);
                    if path.is_file() {
                        let base_lines = (change.index != '?' && change.worktree != '?')
                            .then(|| git_base_file(&self.search_root, &change.path))
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
                self.status_message = "Commit message · Enter commit · Esc cancel".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('b')) => self.load_blame(),
            (KeyModifiers::NONE, KeyCode::Char('l')) => self.load_history(),
            (KeyModifiers::NONE, KeyCode::Char('f')) => self.run_git_remote("fetch"),
            (KeyModifiers::NONE, KeyCode::Char('p')) => self.run_git_remote("pull"),
            (KeyModifiers::SHIFT, KeyCode::Char('P')) => self.run_git_remote("push"),
            _ => self.handle_navigation(key),
        }
        Ok(())
    }

    fn handle_output_key(&mut self, key: KeyEvent) {
        if self.pull_request_comment_mode {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char(character)) => {
                    self.commit_message.push(character)
                }
                (KeyModifiers::NONE, KeyCode::Backspace) => {
                    self.commit_message.pop();
                }
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.pull_request_comment_mode = false;
                    self.commit_message.clear();
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
                    Ok(package) => match PluginRuntime::load(&package) {
                        Ok(runtime) => {
                            self.plugin_runtimes.push(runtime);
                            self.status_message = format!(
                                "Installed and loaded plugin {} {}",
                                entry.name, entry.version
                            );
                        }
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
                if self.diff_split && !self.change_diff.is_empty() =>
            {
                if let Some(change) = self.changes.get(self.change_index).cloned() {
                    let path = self.search_root.join(&change.path);
                    if path.is_file() {
                        match Buffer::open(&path) {
                            Ok(buffer) => {
                                self.open_buffer(buffer);
                                self.diff_base_lines = (change.index != '?'
                                    && change.worktree != '?')
                                    .then(|| git_base_file(&self.search_root, &change.path))
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
            (KeyModifiers::NONE, KeyCode::Char('v')) if !self.change_diff.is_empty() => {
                self.diff_split = !self.diff_split;
                self.status_message = if self.diff_split {
                    "Git diff: split view"
                } else {
                    "Git diff: unified view"
                }
                .to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('m')) => {
                if let Err(error) = self.load_pull_request_comments() {
                    self.status_message = format!("PR comments failed: {error}");
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) if !self.pull_request_comments.is_empty() => {
                if let Err(error) = self.jump_to_pull_request_comment() {
                    self.status_message = format!("PR comment navigation failed: {error}");
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('c'))
                if self.pull_request_target.is_some() && !self.change_diff.is_empty() =>
            {
                self.pull_request_comment_mode = true;
                self.commit_message.clear();
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
        checkout_conflict_side(&self.search_root, &change.path, ours)?;
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
            (KeyModifiers::NONE, KeyCode::Char(character)) => self.commit_message.push(character),
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.commit_message.pop();
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.commit_mode = false;
                self.commit_message.clear();
                self.status_message = "Commit cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                match git_commit(&self.search_root, &self.commit_message) {
                    Ok(()) => {
                        self.commit_mode = false;
                        self.commit_message.clear();
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
        let Some(path) = self.buffer().path() else {
            self.status_message = "Blame unavailable for welcome buffer".to_owned();
            return;
        };
        let Ok(relative) = path.strip_prefix(&self.search_root) else {
            self.status_message = "Blame file is outside workspace".to_owned();
            return;
        };
        self.blame_lines = git_blame(&self.search_root, &relative.to_string_lossy());
        self.status_message = if self.blame_lines.is_empty() {
            "No blame data available".to_owned()
        } else {
            format!("Loaded blame for {} lines", self.blame_lines.len())
        };
    }

    fn load_history(&mut self) {
        self.history = git_history(&self.search_root, 40);
        self.blame_lines.clear();
        self.bottom_tab = BottomTab::Output;
        self.status_message = format!("Loaded {} commits", self.history.len());
    }

    fn run_git_remote(&mut self, operation: &str) {
        if self.git_remote_pending.is_some() {
            self.status_message = "A Git remote operation is already running".to_owned();
            return;
        }
        if !matches!(operation, "fetch" | "pull" | "push") {
            return;
        }
        let root = self.search_root.clone();
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
                    _ => unreachable!("validated Git remote operation"),
                }
                .map_err(|error| error.to_string());
                let _ = sender.send(GitRemoteCompletion {
                    operation: worker_operation,
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

    fn handle_git_remote_failure(&mut self, operation: &str, message: &str) {
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
                username: String::new(),
                password: String::new(),
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
                self.status_message = "New branch · Enter create · Esc cancel".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                if let Some(branch) = self.branches.get(self.branch_index).cloned() {
                    if self.branch.as_deref() == Some(branch.as_str()) {
                        self.status_message = "Cannot delete the current branch".to_owned();
                    } else {
                        match git_delete_branch(&self.search_root, &branch) {
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
                    match git_checkout(&self.search_root, &branch) {
                        Ok(()) => {
                            self.refresh_git_state();
                            self.explorer_paths = self.explorer_entries();
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
    fn handle_branch_name_key(&mut self, key: KeyEvent) -> AppResult<()> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(character)) => self.branch_name.push(character),
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.branch_name.pop();
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.branch_mode = false;
                self.branch_name.clear();
                self.status_message = "Branch creation cancelled".to_owned();
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.branch_name.trim().is_empty() {
                    self.status_message = "Branch name must not be empty".to_owned();
                    return Ok(());
                }
                let branch = self.branch_name.trim().to_owned();
                match git_create_branch(&self.search_root, &branch) {
                    Ok(()) => {
                        self.branch_mode = false;
                        self.branch_name.clear();
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
            .map_or_else(Vec::new, |change| git_diff(&self.search_root, change));
        self.change_hunk_index = self
            .change_hunk_index
            .min(git_hunk_count(&self.change_diff).saturating_sub(1));
        self.output_scroll = 0;
    }

    fn change_hunk_stage(&mut self, should_stage: bool) -> AppResult<()> {
        let Some(change) = self.changes.get(self.change_index).cloned() else {
            return Ok(());
        };
        git_apply_hunk(
            &self.search_root,
            &change,
            self.change_hunk_index,
            should_stage,
        )?;
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
        let changes = self.changes.clone();
        let mut changed = 0usize;
        for change in changes {
            let should_apply = if should_stage {
                change.worktree != ' ' || change.index == '?'
            } else {
                change.index != ' ' && change.index != '?'
            };
            if !should_apply {
                continue;
            }
            if should_stage {
                git_stage(&self.search_root, &change.path)?;
            } else {
                git_unstage(&self.search_root, &change.path)?;
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
        let uri = format!("file://{}", path.to_string_lossy());
        let root_uri = format!("file://{}", self.search_root.to_string_lossy());
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

    fn notify_lsp_saved(&mut self, language: &str, path: &Path) {
        if !self.settings.lsp.enabled || language.is_empty() || !self.lsp.configured(language) {
            return;
        }
        let uri = format!("file://{}", path.to_string_lossy());
        if let Err(error) = self.lsp.save_document(language, &uri) {
            self.status_message = format!("LSP save notification failed: {error}");
        }
    }

    fn notify_lsp_closed(&mut self, language: &str, path: &Path) {
        if !self.settings.lsp.enabled || language.is_empty() || !self.lsp.configured(language) {
            return;
        }
        let uri = format!("file://{}", path.to_string_lossy());
        if let Err(error) = self.lsp.close_document(language, &uri) {
            self.status_message = format!("LSP close notification failed: {error}");
        }
    }

    fn change_stage(&mut self, should_stage: bool) -> AppResult<()> {
        let Some(path) = self
            .changes
            .get(self.change_index)
            .map(|change| change.path.clone())
        else {
            return Ok(());
        };
        if should_stage {
            git_stage(&self.search_root, &path)?;
            self.status_message = format!("Staged {path}");
        } else {
            git_unstage(&self.search_root, &path)?;
            self.status_message = format!("Unstaged {path}");
        }
        self.refresh_git_state();
        Ok(())
    }
    fn refresh_git_state(&mut self) {
        self.changes = git_status(&self.search_root);
        self.branch = git_branch(&self.search_root);
        self.branches = git_branches(&self.search_root);
        self.branch_index = self
            .branch
            .as_ref()
            .and_then(|current| self.branches.iter().position(|branch| branch == current))
            .unwrap_or(0);
        self.change_index = self.change_index.min(self.changes.len().saturating_sub(1));
        self.refresh_change_diff();
    }
    fn handle_explorer_key(&mut self, key: KeyEvent) -> AppResult<()> {
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
            KeyCode::Char(character) => self.explorer_input.push(character),
            KeyCode::Backspace => {
                self.explorer_input.pop();
            }
            KeyCode::Esc => self.cancel_explorer_input(),
            KeyCode::Enter => self.commit_explorer_input()?,
            _ => {}
        }
        Ok(())
    }

    fn start_explorer_input(&mut self, directory: bool) {
        self.explorer_input.clear();
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
        self.explorer_mode = Some(ExplorerMode::Rename(path));
        self.status_message = "Rename · Enter apply · Esc cancel".to_owned();
    }

    fn start_explorer_delete(&mut self) {
        let Some(path) = self.selected_explorer_path() else {
            self.status_message = "No Explorer item selected".to_owned();
            return;
        };
        self.explorer_input.clear();
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
                    fs::rename(&path, &target)?;
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
            .flat_map(|root| visible_explorer_entries(root, &self.explorer_expanded))
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
    fn handle_terminal_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            self.open_terminal();
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
        match key.code {
            KeyCode::PageUp => {
                self.terminal_mut().scroll(3);
                return Ok(());
            }
            KeyCode::PageDown => {
                self.terminal_mut().scroll(-3);
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
            (KeyModifiers::NONE, KeyCode::Up) => self.agent.write_bytes(b"\x1b[A")?,
            (KeyModifiers::NONE, KeyCode::Down) => self.agent.write_bytes(b"\x1b[B")?,
            (KeyModifiers::NONE, KeyCode::Right) => self.agent.write_bytes(b"\x1b[C")?,
            (KeyModifiers::NONE, KeyCode::Left) => self.agent.write_bytes(b"\x1b[D")?,
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
        let (command, cwd) = self
            .settings
            .agent
            .providers
            .get(&self.agent_provider)
            .map(|provider| (provider.command.clone(), provider.cwd.clone()))
            .unwrap_or_else(|| {
                (
                    self.settings.agent.command.clone(),
                    self.settings.agent.cwd.clone(),
                )
            });
        self.agent = TerminalState::with_command(command, cwd.map(PathBuf::from));
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
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                self.move_search_result(1);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.move_search_result(-1);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                self.replace_mode = !self.replace_mode;
                return;
            }
            (KeyModifiers::NONE, KeyCode::Char(c)) if self.replace_mode => {
                self.replace_query.push(c)
            }
            (KeyModifiers::NONE, KeyCode::Char(c)) => self.search_query.push(c),
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if self.replace_mode {
                    self.replace_query.pop();
                } else {
                    self.search_query.pop();
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if !self.search_results.is_empty() {
                    self.search_index = (self.search_index + 1).min(self.search_results.len() - 1);
                }
                return;
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.search_index = self.search_index.saturating_sub(1);
                return;
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.focus = Focus::Editor;
                return;
            }
            (modifiers, KeyCode::Enter)
                if self.replace_mode
                    && modifiers.contains(KeyModifiers::CONTROL)
                    && modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Err(error) = self.replace_workspace_all() {
                    self.status_message = format!("Workspace replace failed: {error}");
                }
                return;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.replace_mode {
                    let query = self.search_query.clone();
                    let replacement = self.replace_query.clone();
                    let count = self.buffer_mut().replace_all(&query, &replacement);
                    self.sync_lsp_for_buffer();
                    self.status_message = format!("Replaced {count} occurrence(s)");
                    return;
                }
                if let Some(result) = self.search_results.get(self.search_index).cloned() {
                    match Buffer::open(&result.path) {
                        Ok(mut buffer) => {
                            buffer.set_cursor(
                                result.line.saturating_sub(1),
                                result.column.saturating_sub(1),
                            );
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
                } else {
                    self.status_message = format!("{} matches", self.search_results.len());
                }
                return;
            }
            _ => {
                self.handle_navigation(key);
                return;
            }
        }
        self.refresh_search();
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
            if active_path.as_ref() == Some(&path) {
                if self.search_regex {
                    let content = self.file_lines().join("\n");
                    let (updated, count) =
                        replace_with_options(&content, &query, &replacement, true);
                    if count > 0 {
                        let last_line = self.file_lines().len().saturating_sub(1);
                        let last_col = self.file_lines()[last_line].chars().count();
                        self.buffer_mut().replace_ranges(
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
                    replaced += self
                        .buffer_mut()
                        .replace_all_smart_case(&query, &replacement);
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
    fn refresh_search(&mut self) {
        self.refresh_search_results();
        self.status_message = if self.search_query.is_empty() {
            "Search workspace".to_owned()
        } else {
            format!("{} matches", self.search_results.len())
        };
    }
    fn refresh_search_results(&mut self) {
        self.search_results = search_workspace_roots_with_options(
            &self.workspace_roots,
            &self.search_query,
            &self.settings.search.include,
            &self.settings.search.exclude,
            self.search_regex,
        );
        let active_path = self.buffer().path().map(Path::to_path_buf);
        if let Some(path) = active_path {
            self.search_results.retain(|result| result.path != path);
            let text = self.buffer().lines().join("\n");
            self.search_results.extend(search_text_with_options(
                &path,
                &text,
                &self.search_query,
                self.search_regex,
            ));
        }
        self.search_index = 0;
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
    fn handle_editor_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if self.plugin_file_view.is_some() {
            self.handle_plugin_file_key(key);
            return Ok(());
        }
        if self.rename_mode {
            return self.handle_rename_key(key);
        }
        let initial_cursor = self.cursor();
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.close_active_tab();
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
                KeyCode::Left => {
                    self.horizontal_scroll = self.horizontal_scroll.saturating_sub(4);
                    return Ok(());
                }
                KeyCode::Right => {
                    self.horizontal_scroll = self.horizontal_scroll.saturating_add(4);
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers == (KeyModifiers::ALT | KeyModifiers::SHIFT) {
            match key.code {
                KeyCode::Up => {
                    self.move_selected_lines(true);
                    return Ok(());
                }
                KeyCode::Down => {
                    self.move_selected_lines(false);
                    return Ok(());
                }
                KeyCode::Char('s') => {
                    self.sort_selected_lines(false);
                    return Ok(());
                }
                KeyCode::Char('u') => {
                    self.transform_selected_lines(true);
                    return Ok(());
                }
                KeyCode::Char('l') => {
                    self.transform_selected_lines(false);
                    return Ok(());
                }
                KeyCode::Char('i') => {
                    self.split_selection_into_lines();
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
            )
        {
            self.selection_anchor.get_or_insert(self.buffer().cursor());
            match key.code {
                KeyCode::Left => self.buffer_mut().move_cursor(0, -1),
                KeyCode::Right => self.buffer_mut().move_cursor(0, 1),
                KeyCode::Up => self.move_cursor_vertically(-1),
                KeyCode::Down => self.move_cursor_vertically(1),
                KeyCode::Home => self.buffer_mut().move_home(),
                KeyCode::End => self.buffer_mut().move_end(),
                _ => {}
            }
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
                        return Ok(());
                    }
                }
                KeyCode::Char('v') if !self.clipboard.is_empty() => {
                    if !self.secondary_selections.is_empty() {
                        let clipboard = self.clipboard.clone();
                        self.paste_at_selections(&clipboard);
                        return Ok(());
                    }
                    if !self.secondary_cursors.is_empty() {
                        let clipboard = self.clipboard.clone();
                        self.paste_at_cursors(&clipboard);
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
                    return Ok(());
                }
                _ => {}
            }
        }
        if !self.secondary_selections.is_empty() {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Char(character)) => {
                    self.replace_active_selections(&character.to_string());
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let newline = self.newline_with_indent();
                    self.replace_active_selections(&newline);
                    return Ok(());
                }
                (KeyModifiers::NONE, KeyCode::Backspace | KeyCode::Delete) => {
                    self.replace_active_selections("");
                    return Ok(());
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
        {
            match key.code {
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
                key.code,
                KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace
            )
        {
            if let Some(anchor) = self.selection_anchor.take() {
                let cursor = self.buffer().cursor();
                self.buffer_mut().delete_between(anchor, cursor);
            }
        }
        let modifies_document = matches!(
            (key.modifiers, key.code),
            (KeyModifiers::CONTROL, KeyCode::Char('s' | 'z' | 'y'))
                | (KeyModifiers::CONTROL, KeyCode::Backspace | KeyCode::Delete)
                | (KeyModifiers::NONE, KeyCode::Char(_))
                | (KeyModifiers::NONE, KeyCode::Enter)
                | (KeyModifiers::NONE, KeyCode::Backspace)
                | (KeyModifiers::NONE, KeyCode::Delete)
        ) || (key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('k')))
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('/'));
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char(' ')) => {
                self.request_lsp("textDocument/completion")?
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
                if self.buffer().has_unresolved_conflicts() {
                    self.status_message = "Save blocked: resolve merge conflicts first".to_owned();
                    return Ok(());
                }
                let editor = self.settings.editor.clone();
                if editor.format_on_save {
                    if let Err(error) = self.request_lsp_edits("textDocument/formatting") {
                        self.status_message = format!("Format-on-save skipped: {error}");
                    }
                }
                self.buffer_mut().save_with_options(
                    editor.trim_trailing_whitespace,
                    editor.insert_final_newline,
                )?;
                if let Some(path) = self.buffer().path().map(Path::to_path_buf) {
                    let language = self.language().to_owned();
                    self.notify_lsp_saved(&language, &path);
                }
                self.emit_plugin_event("document.save");
                self.refresh_git_state();
                self.status_message = "Saved".to_owned();
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
            (KeyModifiers::NONE, KeyCode::Up) => self.move_cursor_vertically(-1),
            (KeyModifiers::NONE, KeyCode::Down) => self.move_cursor_vertically(1),
            (KeyModifiers::NONE, KeyCode::Home) => self.buffer_mut().move_home(),
            (KeyModifiers::NONE, KeyCode::End) => self.buffer_mut().move_end(),
            _ => self.handle_navigation(key),
        }
        if modifies_document {
            self.sync_lsp_for_buffer();
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
                    let path = self.search_root.join(&change.path);
                    let base = (change.index != '?' && change.worktree != '?')
                        .then(|| git_base_file(&self.search_root, &change.path))
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
            if self.settings.editor.use_tabs {
                "\t".to_owned()
            } else {
                " ".repeat(self.settings.editor.indent_size.max(1))
            }
        } else {
            String::new()
        };
        format!("\n{leading}{extra}")
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
        let line = (primary.line as isize + line_delta)
            .clamp(0, self.buffer().lines().len().saturating_sub(1) as isize)
            as usize;
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
            let mut fenced = false;
            let mut opening_tags = 0;
            for (index, line) in lines.iter().enumerate().take(start + 1) {
                let (opens, _) = markdown_details_tags(line, &mut fenced);
                if index == start {
                    opening_tags = opens;
                }
            }
            if opening_tags == 0 {
                return self.fold_end_by_indentation(lines, start);
            }
            let mut depth = 0usize;
            fenced = false;
            for (index, line) in lines.iter().enumerate().skip(start) {
                let (opens, closes) = markdown_details_tags(line, &mut fenced);
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
        let mut inherited_selection = false;
        if let Some(anchor) = self.selection_anchor {
            let selected = self.buffer().text_between(anchor, self.buffer().cursor());
            if !selected.is_empty() && !selected.contains('\n') {
                self.search_query = selected;
                inherited_selection = true;
            }
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
            self.status_message = format!("{} matches", self.search_results.len());
        } else {
            self.refresh_search();
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
        let uri = format!("file://{}", path.to_string_lossy());
        let cursor = self.buffer().cursor();
        let character = utf16_column(self.file_lines()[cursor.line].as_str(), cursor.column);
        let params = if method == "textDocument/completion" {
            json!({"textDocument":{"uri":uri},"position":{"line":cursor.line,"character":character},"context":{"triggerKind":1}})
        } else {
            json!({"textDocument":{"uri":uri},"position":{"line":cursor.line,"character":character}})
        };
        let Some(result) = self.lsp.request(&language, method, params)? else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        match method {
            "textDocument/completion" => {
                let items = completion_labels(&result);
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
        let uri = format!("file://{}", path.to_string_lossy());
        let cursor = self.buffer().cursor();
        let character = utf16_column(self.file_lines()[cursor.line].as_str(), cursor.column);
        let result = self.lsp.request(
            &language,
            "textDocument/rename",
            json!({
                "textDocument":{"uri":uri.clone()},
                "position":{"line":cursor.line,"character":character},
                "newName":new_name
            }),
        )?;
        let Some(result) = result else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        let summary = workspace_edit_summary(&result);
        if summary.is_empty() {
            self.status_message = "LSP rename returned no edits".to_owned();
        } else {
            self.lsp_popup = Some(LspPopup::RenamePreview {
                uri,
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
        let uri = format!("file://{}", path.to_string_lossy());
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
        let params = if request_method == "textDocument/formatting" {
            json!({
                "textDocument":{"uri":uri.clone()},
                "options":{"tabSize":4,"insertSpaces":true}
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
                "options":{"tabSize":4,"insertSpaces":true}
            })
        } else {
            let character = utf16_column(self.file_lines()[cursor.line].as_str(), cursor.column);
            json!({
                "textDocument":{"uri":uri},
                "range":{"start":{"line":cursor.line,"character":0},"end":{"line":cursor.line,"character":character}},
                "context":{"diagnostics":[]}
            })
        };
        let Some(result) = self.lsp.request(&language, request_method, params)? else {
            self.status_message = format!("No LSP server configured for {language}");
            return Ok(());
        };
        if method == "textDocument/codeAction" {
            let actions: Vec<_> = result
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|action| {
                    let edit = action.get("edit")?.clone();
                    let title = action
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Code action")
                        .to_owned();
                    Some((title, edit))
                })
                .collect();
            if actions.is_empty() {
                self.status_message = "LSP codeAction returned no editable actions".to_owned();
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
            let changed = self.apply_workspace_edit(&uri, &actions[0].1)?;
            self.status_message = format!("LSP codeAction applied {changed} edit(s)");
            return Ok(());
        }
        let changed = apply_lsp_text_edits(self.buffer_mut(), &uri, &result);
        if changed > 0 {
            self.sync_lsp_for_buffer();
            self.status_message = format!("LSP {} applied {} edit(s)", method, changed);
        } else {
            self.status_message = format!("LSP {} returned no edits", method);
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
                    if !old_exists || uri_to_path(current_uri).as_deref() == Some(old) {
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

        let mut changed = 0;
        for (uri, edits) in plans {
            if uri == current_uri {
                changed += apply_parsed_lsp_text_edits(self.buffer_mut(), &edits);
            } else if let Some(path) = uri_to_path(&uri) {
                let mut buffer = Buffer::open(&path)?;
                let count = apply_parsed_lsp_text_edits(&mut buffer, &edits);
                if count > 0 {
                    buffer.save()?;
                    changed += count;
                }
            }
        }
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
                    if overwrite && new.exists() {
                        fs::remove_file(&new)?;
                    }
                    fs::rename(&old, &new)?;
                    changed += 1;
                }
                ResourceOperation::Delete { path, .. } => {
                    if path.exists() {
                        fs::remove_file(&path)?;
                        changed += 1;
                    }
                }
            }
        }
        if changed > 0 {
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
        let lines = self.file_lines();
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
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let text = items.get(*selected).cloned();
                    self.close_lsp_popup();
                    if let Some(text) = text {
                        self.buffer_mut().insert(&text);
                        self.sync_lsp_for_buffer();
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
                    let edit = edits.get(*selected).cloned();
                    self.close_lsp_popup();
                    if let Some(edit) = edit {
                        let uri = self
                            .buffer()
                            .path()
                            .map(|path| format!("file://{}", path.to_string_lossy()));
                        if let Some(uri) = uri {
                            let changed = self.apply_workspace_edit(&uri, &edit)?;
                            self.status_message =
                                format!("LSP codeAction applied {changed} edit(s)");
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
            LspPopup::RenamePreview { uri, edit, .. } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let uri = uri.clone();
                    let edit = edit.clone();
                    self.close_lsp_popup();
                    let changed = self.apply_workspace_edit(&uri, &edit)?;
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
                self.bottom_visible = !self.bottom_visible;
                self.status_message = if self.bottom_visible {
                    "Bottom panel shown"
                } else {
                    "Bottom panel hidden"
                }
                .to_owned();
            }
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
        self.focus = match tab {
            SidebarTab::Explorer => Focus::Explorer,
            SidebarTab::Search => Focus::Search,
            SidebarTab::Changes => Focus::Changes,
            SidebarTab::Branches => Focus::Branches,
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

fn list_directory(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
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

fn visible_explorer_entries(root: &Path, expanded: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut output = Vec::new();
    fn visit(path: &Path, expanded: &BTreeSet<PathBuf>, output: &mut Vec<PathBuf>) {
        for child in list_directory(path) {
            output.push(child.clone());
            if child.is_dir() && expanded.contains(&child) {
                visit(&child, expanded, output);
            }
        }
    }
    visit(root, expanded, &mut output);
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AgentSession {
    provider: String,
    input: String,
    screen: Vec<String>,
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
        ("editor.undo", "Undo"),
        ("editor.redo", "Redo"),
        ("editor.find", "Find"),
        ("editor.gotoLine", "Go to Line"),
        ("editor.joinLines", "Join Lines"),
        ("editor.sortLines", "Sort Lines"),
        ("editor.uppercase", "Uppercase Lines"),
        ("editor.lowercase", "Lowercase Lines"),
        ("editor.splitSelectionLines", "Split Selection into Lines"),
        ("editor.duplicateLine", "Duplicate Line"),
        ("editor.deleteLine", "Delete Line"),
        ("editor.deleteWordBackward", "Delete Word Backward"),
        ("editor.deleteWordForward", "Delete Word Forward"),
        ("editor.toggleComment", "Toggle Comment"),
        ("view.toggleSidebar", "Toggle Sidebar"),
        ("view.toggleTerminal", "Toggle Terminal"),
        ("view.toggleAgent", "Toggle Agent"),
        ("view.switchTheme", "Switch Theme"),
        ("workspace.openFile", "Open File"),
        ("settings.open", "Open Settings"),
        ("plugin.refreshRegistry", "Refresh Plugin Registry"),
    ]
    .into_iter()
    .map(|(id, title)| (id.to_owned(), title.to_owned()))
    .collect()
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
    serde_json::from_str(&contents).ok()
}

fn save_agent_session(path: &Path, session: &AgentSession) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(session)
        .map_err(|error| crate::error::AppError::Workspace(error.to_string()))?;
    fs::write(path, contents)?;
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

fn completion_labels(result: &serde_json::Value) -> Vec<String> {
    result
        .get("items")
        .and_then(serde_json::Value::as_array)
        .or_else(|| result.as_array())
        .map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|item| item.get("label").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .take(20)
                .collect()
        })
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
        let kind = change.get("kind").and_then(serde_json::Value::as_str);
        let path = match kind {
            Some("create") | Some("rename") => change
                .get("uri")
                .or_else(|| change.get("newUri"))
                .and_then(serde_json::Value::as_str)
                .and_then(uri_to_path),
            _ => None,
        };
        let Some(path) = path else { continue };
        if path.exists() {
            conflicts.push(format!(
                "resource target already exists: {}",
                path.display()
            ));
        }
    }
    conflicts
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
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

#[derive(Debug, Deserialize)]
struct WorkspaceFile {
    #[serde(default)]
    folders: Vec<WorkspaceFolder>,
}

#[derive(Debug, Deserialize)]
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

fn markdown_details_tags(line: &str, fenced: &mut bool) -> (usize, usize) {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        *fenced = !*fenced;
        return (0, 0);
    }
    if *fenced {
        return (0, 0);
    }

    let bytes = line.as_bytes();
    let mut inline_code = false;
    let mut opens = 0;
    let mut closes = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 96 && (index == 0 || bytes[index - 1] != 92) {
            inline_code = !inline_code;
            index += 1;
            continue;
        }
        if !inline_code {
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

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }
    #[test]
    fn tab_cycles_visible_focus() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE))
            .expect("key");
        assert_eq!(app.focus(), Focus::Terminal);
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
    fn split_plugin_diff_preserves_sides() {
        let text = "diff --git a/a b/a\n-old\n+new\n context";
        let rendered = split_unified_diff_text(text);
        assert!(rendered.contains("-old"));
        assert!(rendered.contains("+new"));
        assert!(rendered.contains("context"));
        assert!(rendered.lines().all(|line| line.contains('│')));
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
            username: String::new(),
            password: String::new(),
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
        let prompt = app.git_credential_prompt().expect("prompt");
        assert_eq!(prompt.username, "u");
        assert_eq!(prompt.password, "secret");
        assert!(prompt.password_focus);
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel credentials");
        assert!(app.git_credential_prompt().is_none());
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
    fn mouse_selection_updates_editor_cursor_and_selection() {
        let mut app = App::new(&[]).expect("welcome app");
        app.mouse_down(Position { line: 0, column: 1 }, false);
        app.mouse_drag(Position { line: 2, column: 2 });
        app.mouse_up();
        assert_eq!(app.cursor(), Position { line: 2, column: 2 });
        assert_eq!(app.selection_lines(), Some((0, 2)));
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
            KeyCode::Char('t'),
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
    fn agent_install_completion_restarts_the_selected_provider_session() {
        let mut app = App::new(&[]).expect("welcome app");
        app.settings.agent.providers.insert(
            "test".to_owned(),
            crate::config::AgentProviderConfig {
                extra: std::collections::BTreeMap::new(),
                name: "Test".to_owned(),
                command: vec!["sh".to_owned()],
                cwd: None,
                install: Some("echo install".to_owned()),
                tools: Vec::new(),
            },
        );
        app.agent_provider = "test".to_owned();
        app.agent_install_pending = true;
        app.poll_background();
        assert!(!app.agent_install_pending);
        assert!(app.status_message().contains("session restarted"));
    }

    #[test]
    fn terminal_links_reject_non_http_protocols() {
        let mut app = App::new(&[]).expect("welcome app");
        app.open_terminal_link("file:///etc/passwd");
        assert!(app.status_message().contains("only http(s)"));
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
    fn bracketed_paste_routes_to_editor_and_search_inputs() {
        let mut app = App::new(&[]).expect("welcome app");
        app.handle_paste("你好\nworld").expect("editor paste");
        assert_eq!(app.file_lines()[0], "你好");
        app.focus = Focus::Search;
        app.handle_paste("needle").expect("search paste");
        assert_eq!(app.search_query(), "needle");
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
    fn markdown_details_tags_ignore_inline_code_and_fenced_code() {
        let mut fenced = false;
        assert_eq!(
            markdown_details_tags("中文 </details>", &mut fenced),
            (0, 1)
        );
        assert_eq!(
            markdown_details_tags("Use `</details>`", &mut fenced),
            (0, 0)
        );
        assert_eq!(markdown_details_tags("```html", &mut fenced), (0, 0));
        assert_eq!(markdown_details_tags("</details>", &mut fenced), (0, 0));
        assert_eq!(markdown_details_tags("```", &mut fenced), (0, 0));
        assert_eq!(markdown_details_tags("<details>", &mut fenced), (1, 0));
        assert_eq!(markdown_details_tags("</details>", &mut fenced), (0, 1));
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
    fn branch_creation_mode_captures_name_without_switching_focus() {
        let mut app = App::new(&[]).expect("welcome app");
        app.focus = Focus::Branches;
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("open branch input");
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE))
            .expect("branch character");
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::NONE))
            .expect("branch character");
        assert!(app.branch_mode());
        assert_eq!(app.branch_name(), "fe");
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
        assert!(app.changes.iter().all(|change| change.index != ' '));
        app.handle_key(key(KeyCode::Char('A'), KeyModifiers::SHIFT))
            .expect("unstage all");
        assert!(app.changes.iter().all(|change| change.index == ' '));
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
    fn extracts_completion_labels_and_hover_text() {
        let result = serde_json::json!({
            "items": [{"label":"alpha"},{"label":"beta"}],
            "contents": [{"value":"fn alpha()"}]
        });
        assert_eq!(completion_labels(&result), vec!["alpha", "beta"]);
        assert_eq!(hover_text(&result), "fn alpha()");
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
    fn executable_lookup_checks_path_without_launching_process() {
        assert!(executable_available("sh"));
        assert!(!executable_available("mtide-command-that-does-not-exist"));
    }

    #[test]
    fn agent_session_round_trips_provider_input_and_screen() {
        let path = std::env::temp_dir().join(format!("mtide-agent-session-{}", std::process::id()));
        let session = AgentSession {
            provider: "local".into(),
            input: "review".into(),
            screen: vec!["ready".into(), "output".into()],
        };
        save_agent_session(&path, &session).expect("save session");
        assert_eq!(load_agent_session(&path), Some(session));
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
            },
        );
        let session = AgentSession {
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
        let mut app = App::new(&[root.to_string_lossy().into()]).expect("app");
        app.settings.agent.tools = vec![
            "read_file".into(),
            "list_files".into(),
            "write_file".into(),
            "search".into(),
        ];
        let result = app
            .agent_tool_call("read_file", &json!({"path": "note.txt"}))
            .expect("read tool");
        assert_eq!(result["content"], "hello");
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
        let search = app
            .agent_tool_call(
                "search",
                &json!({"path": ".", "query": "updated", "limit": 5}),
            )
            .expect("search tool");
        assert_eq!(search["results"][0]["line"], 1);
        assert!(app
            .agent_tool_call("shell", &json!({"path": "note.txt"}))
            .is_err());
        assert!(app
            .agent_tool_call("read_file", &json!({"path": "/etc/hosts"}))
            .is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
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
    fn editable_diff_markers_follow_current_line_order() {
        let base = vec!["same".into(), "old".into(), "tail".into()];
        let current = vec!["same".into(), "new".into(), "tail".into(), "added".into()];
        assert_eq!(
            editor_diff_markers(&base, &current),
            vec![None, Some('+'), None, Some('+')]
        );
    }
}
