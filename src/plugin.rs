use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{error::AppResult, terminal::TerminalScreen};
use mlua::LuaSerdeExt;
use mlua::{Function, Lua, MultiValue, RegistryKey, Table, Value as LuaValue, Variadic};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::{cell::RefCell, rc::Rc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const SUPPORTED_API: u32 = 2;
const PLUGIN_TERMINAL_OUTPUT_LIMIT: usize = 1_048_576;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default = "default_api")]
    pub api: u32,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_permissions")]
    pub permissions: BTreeMap<String, bool>,
    #[serde(skip)]
    pub network_http_hosts: Vec<String>,
    #[serde(default)]
    pub grammars: Vec<Grammar>,
    #[serde(default)]
    pub themes: Vec<ThemeContribution>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Grammar {
    pub path: String,
    #[serde(default)]
    pub language: String,
    #[serde(default, rename = "fileTypes")]
    pub file_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ThemeContribution {
    Path(String),
    Descriptor {
        path: String,
        #[serde(default)]
        label: Option<String>,
    },
}

impl ThemeContribution {
    pub fn path(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Descriptor { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginPackage {
    pub root: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, Default, Clone)]
pub struct PluginRegistration {
    pub sidebar: bool,
    pub bottom: bool,
    pub commands: Vec<PluginCommand>,
    pub keybindings: Vec<PluginKeybinding>,
    pub status_items: Vec<PluginStatusItem>,
    pub panels: Vec<PluginPanel>,
    pub file_handlers: Vec<PluginFileHandler>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginKeybinding {
    pub key: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStatusItem {
    pub id: String,
    pub text: String,
    pub side: String,
    pub priority: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPanel {
    pub id: String,
    pub title: String,
    pub location: PluginPanelLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginPanelLocation {
    Sidebar,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginPanelRequest {
    Show {
        location: PluginPanelLocation,
        focus: bool,
    },
    OpenDrawer {
        side: String,
        width: u16,
        min_width: u16,
    },
    CloseDrawer,
    OpenTab {
        id: String,
        title: String,
        focus: bool,
    },
    CloseTab {
        id: String,
    },
    FocusTab {
        id: String,
    },
    OpenFile {
        path: String,
        line: Option<usize>,
        readonly: bool,
    },
    ClipboardWrite {
        text: String,
    },
    Confirm {
        id: u64,
        message: String,
    },
    ShowInfo {
        title: String,
        entries: Vec<(String, String)>,
    },
    CommandLineShow {
        id: u64,
        prefix: String,
        text: String,
    },
    CommandLineHide,
    CommandLineSetText {
        text: String,
    },
    OpenDialog {
        id: u64,
        title: String,
        width: u16,
        height: u16,
    },
    CloseDialog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginEditorRequest {
    BeginUndoGroup,
    EndUndoGroup,
    Insert {
        line: usize,
        col: usize,
        text: String,
    },
    SetLine {
        line: usize,
        text: String,
    },
    Replace {
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
        text: String,
    },
    SetCursor {
        line: usize,
        col: usize,
    },
    SetSelection {
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    },
    ClearSelection,
    AddCursor {
        line: usize,
        col: usize,
    },
    ClearCursors,
    ScrollTo {
        line: usize,
    },
    ScrollBy {
        delta: isize,
    },
    SetSearch {
        pattern: String,
        use_regex: bool,
    },
    ClearSearch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginEventArg {
    Text(String),
    Number(usize),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDiagnostic {
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub severity: u8,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBookmarkEvent {
    pub path: String,
    pub line: usize,
    pub action: String,
    pub icon: String,
    pub style: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAutomationTask {
    pub id: u64,
    pub action: String,
    pub args: String,
}

#[derive(Debug)]
enum RpcTransport {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
}

impl RpcTransport {
    fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
        }
    }

    fn shutdown(&self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.shutdown(std::net::Shutdown::Both),
            Self::Tcp(stream) => stream.shutdown(std::net::Shutdown::Both),
        }
    }
}

impl Read for RpcTransport {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for RpcTransport {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

#[derive(Debug)]
enum RpcEventKind {
    Response {
        id: u64,
        message: JsonValue,
    },
    Notification {
        method: String,
        params: JsonValue,
    },
    Request {
        id: u64,
        method: String,
        params: JsonValue,
    },
    Closed,
}

#[derive(Debug)]
struct RpcEvent {
    connection_id: u64,
    kind: RpcEventKind,
}

#[derive(Debug)]
struct RpcRegistration {
    writer: Arc<Mutex<RpcTransport>>,
    on_notification: Option<RegistryKey>,
    on_request: Option<RegistryKey>,
    on_close: Option<RegistryKey>,
}

#[derive(Debug)]
struct PluginAutomationRequest {
    task: PluginAutomationTask,
    callback: RegistryKey,
}

type PluginBookmarks = BTreeMap<String, BTreeMap<usize, (String, String)>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFileHandler {
    pub id: String,
    pub title: String,
    pub extensions: Vec<String>,
}

#[derive(Debug)]
pub struct PluginRuntime {
    _lua: Lua,
    registration: Rc<RefCell<PluginRegistration>>,
    logs: Rc<RefCell<Vec<String>>>,
    command_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    event_callbacks: Rc<RefCell<BTreeMap<String, Vec<RegistryKey>>>>,
    panel_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    file_handler_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    file_close_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    next_file_close_id: Rc<RefCell<u64>>,
    last_file_close_id: RefCell<Option<u64>>,
    last_file_title: RefCell<Option<String>>,
    async_rx: Receiver<AsyncCompletion>,
    async_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    net_rx: Receiver<NetCompletion>,
    net_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    spawn_rx: Receiver<SpawnEvent>,
    spawn_callbacks: Rc<RefCell<BTreeMap<u64, SpawnRegistration>>>,
    panel_requests: Rc<RefCell<Vec<PluginPanelRequest>>>,
    clipboard_text: Rc<RefCell<String>>,
    editor_snapshot: Rc<RefCell<PluginEditorSnapshot>>,
    editor_requests: Rc<RefCell<Vec<PluginEditorRequest>>>,
    diagnostics: Rc<RefCell<BTreeMap<String, Vec<PluginDiagnostic>>>>,
    bookmarks: Rc<RefCell<PluginBookmarks>>,
    bookmark_events: Rc<RefCell<Vec<PluginBookmarkEvent>>>,
    timers: Rc<RefCell<BTreeMap<u64, PluginTimer>>>,
    automation_requests: Rc<RefCell<Vec<PluginAutomationRequest>>>,
    automation_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    dialog_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    command_line_callbacks: Rc<RefCell<BTreeMap<u64, PluginCommandLineCallbacks>>>,
    command_line_active: Rc<RefCell<bool>>,
    custom_dialog_callbacks: Rc<RefCell<BTreeMap<u64, PluginCustomDialogCallbacks>>>,
    dialog_widgets: Rc<RefCell<BTreeMap<u64, PluginDialogWidgetState>>>,
    rpc_rx: Receiver<RpcEvent>,
    rpc_connections: Rc<RefCell<BTreeMap<u64, RpcRegistration>>>,
    rpc_pending: Rc<RefCell<BTreeMap<(u64, u64), RegistryKey>>>,
    open_tabs: Rc<RefCell<BTreeMap<String, Option<RegistryKey>>>>,
}

#[derive(Debug)]
struct PluginTimer {
    callback: RegistryKey,
    next: Instant,
    interval: Option<Duration>,
}

#[derive(Debug)]
struct PluginCommandLineCallbacks {
    on_change: Option<RegistryKey>,
    on_submit: Option<RegistryKey>,
    on_cancel: Option<RegistryKey>,
    on_key: Option<RegistryKey>,
}

#[derive(Debug)]
struct PluginCustomDialogCallbacks {
    on_close: Option<RegistryKey>,
}

#[derive(Debug, Default)]
struct PluginDialogWidgetState {
    input_text: String,
    input_initialized: bool,
    input_on_change: Option<RegistryKey>,
    button_label: Option<String>,
    button_on_click: Option<RegistryKey>,
    focus: PluginDialogFocus,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PluginDialogFocus {
    #[default]
    Input,
    Button,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginEditorSnapshot {
    pub lines: Vec<String>,
    pub path: String,
    pub file_name: String,
    pub language: String,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub cursors: Vec<(usize, usize)>,
    pub selection: Option<(usize, usize, usize, usize)>,
    pub selection_text: String,
}

#[derive(Debug)]
struct AsyncCompletion {
    id: u64,
    result: Result<(std::process::Output, bool), String>,
}

#[derive(Debug, Clone)]
struct NetResponse {
    status: u16,
    body: String,
    headers: BTreeMap<String, String>,
    error: Option<String>,
}

#[derive(Debug)]
struct NetCompletion {
    id: u64,
    response: NetResponse,
}

#[derive(Debug)]
enum SpawnEventKind {
    Stdout(String),
    Stderr(String),
    Exit(Option<i32>),
    StreamClosed { stderr: bool },
}

#[derive(Debug)]
struct SpawnEvent {
    id: u64,
    kind: SpawnEventKind,
}

#[derive(Debug)]
struct SpawnRegistration {
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    stdout_open: bool,
    stderr_open: bool,
    exited: bool,
}

#[derive(Debug)]
struct SpawnHandle {
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pid: u32,
}

#[derive(Debug, Default)]
struct PluginTerminalManager {
    next_id: u64,
    sessions: BTreeMap<String, PluginTerminalSession>,
}

struct PluginTerminalSession {
    label: String,
    _master: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
    screen: Arc<Mutex<TerminalScreen>>,
    columns: u16,
    rows: u16,
}

fn append_terminal_output(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(bytes);
    if output.len() > PLUGIN_TERMINAL_OUTPUT_LIMIT {
        let excess = output.len() - PLUGIN_TERMINAL_OUTPUT_LIMIT;
        output.drain(..excess);
    }
}

fn spawn_reader_thread<R: Read + Send + 'static>(
    id: u64,
    mut reader: R,
    events: Sender<SpawnEvent>,
    stderr: bool,
) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    let text = String::from_utf8_lossy(&buffer[..size]).into_owned();
                    let kind = if stderr {
                        SpawnEventKind::Stderr(text)
                    } else {
                        SpawnEventKind::Stdout(text)
                    };
                    if events.send(SpawnEvent { id, kind }).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = events.send(SpawnEvent {
            id,
            kind: SpawnEventKind::StreamClosed { stderr },
        });
    });
}

fn http_options(
    options: Option<Table>,
) -> Result<(BTreeMap<String, String>, Option<String>), String> {
    let Some(options) = options else {
        return Ok((BTreeMap::new(), None));
    };
    let mut headers = BTreeMap::new();
    if let Some(table) = options
        .get::<Option<Table>>("headers")
        .map_err(|error| error.to_string())?
    {
        for pair in table.pairs::<String, String>() {
            let (name, value) = pair.map_err(|error| error.to_string())?;
            headers.insert(name, value);
        }
    }
    let body = options
        .get::<Option<String>>("body")
        .map_err(|error| error.to_string())?;
    Ok((headers, body))
}

fn net_async_args(lua: &Lua, args: MultiValue) -> mlua::Result<(Option<Table>, Function)> {
    let mut values: Vec<_> = args.into_iter().collect();
    let Some(LuaValue::Function(callback)) = values.pop() else {
        return Err(mlua::Error::external("network callback is required"));
    };
    let options = match values.pop() {
        Some(LuaValue::Table(table)) => Some(table),
        Some(_) => return Err(mlua::Error::external("network options must be a table")),
        None => None,
    };
    let _ = lua;
    Ok((options, callback))
}

fn http_request(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&str>,
    allowed_hosts: &[String],
) -> NetResponse {
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => parsed,
        Ok(_) => {
            return NetResponse {
                status: 0,
                body: String::new(),
                headers: BTreeMap::new(),
                error: Some("only http and https URLs are allowed".to_owned()),
            };
        }
        Err(error) => {
            return NetResponse {
                status: 0,
                body: String::new(),
                headers: BTreeMap::new(),
                error: Some(format!("invalid URL: {error}")),
            };
        }
    };
    let redirect_hosts = allowed_hosts.to_owned();
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.stop();
            }
            if network_target_allowed(attempt.url().as_str(), &redirect_hosts) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return NetResponse {
                status: 0,
                body: String::new(),
                headers: BTreeMap::new(),
                error: Some(error.to_string()),
            };
        }
    };
    let mut request = match method {
        "GET" => client.get(parsed),
        "POST" => client.post(parsed),
        _ => unreachable!(),
    };
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if let Some(body) = body {
        request = request.body(body.to_owned());
    }
    let response = match request.send() {
        Ok(response) => response,
        Err(error) => {
            return NetResponse {
                status: 0,
                body: String::new(),
                headers: BTreeMap::new(),
                error: Some(error.to_string()),
            };
        }
    };
    let status = response.status().as_u16();
    let response_headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    match response.bytes() {
        Ok(bytes) if bytes.len() <= 10 * 1024 * 1024 => NetResponse {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            headers: response_headers,
            error: None,
        },
        Ok(_) => NetResponse {
            status: 0,
            body: String::new(),
            headers: BTreeMap::new(),
            error: Some("response body exceeds 10 MiB".to_owned()),
        },
        Err(error) => NetResponse {
            status: 0,
            body: String::new(),
            headers: BTreeMap::new(),
            error: Some(error.to_string()),
        },
    }
}

fn denied_network_response() -> NetResponse {
    NetResponse {
        status: 0,
        body: String::new(),
        headers: BTreeMap::new(),
        error: Some("network destination is not allowed".to_owned()),
    }
}

fn network_target_allowed(url: &str, allowed_hosts: &[String]) -> bool {
    if allowed_hosts.is_empty() {
        return true;
    }
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let port = url.port_or_known_default();
    allowed_hosts.iter().any(|entry| {
        let (entry_host, entry_port) = if let Some(stripped) = entry.strip_prefix('[') {
            let Some((host, suffix)) = stripped.split_once(']') else {
                return false;
            };
            let port = suffix
                .strip_prefix(':')
                .and_then(|value| value.parse().ok());
            (host, port)
        } else if let Some((host, port)) = entry.rsplit_once(':') {
            if let Ok(port) = port.parse::<u16>() {
                (host, Some(port))
            } else {
                (entry.as_str(), None)
            }
        } else {
            (entry.as_str(), None)
        };
        entry_host.eq_ignore_ascii_case(host)
            && entry_port.is_none_or(|expected| Some(expected) == port)
    })
}

fn net_response_table(lua: &Lua, response: NetResponse) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("status", response.status)?;
    table.set("body", response.body)?;
    if response.error.is_none() {
        let headers = lua.create_table()?;
        for (name, value) in response.headers {
            headers.set(name, value)?;
        }
        table.set("headers", headers)?;
    }
    if let Some(error) = response.error {
        table.set("error", error)?;
    }
    Ok(table)
}

impl std::fmt::Debug for PluginTerminalSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginTerminalSession")
            .field("label", &self.label)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

impl Manifest {
    pub fn title(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
    pub fn validate(&self, root: &Path) -> AppResult<()> {
        if self.name.trim().is_empty() {
            return Err(plugin_error("manifest missing required field: name"));
        }
        if self.entry.is_none() && self.grammars.is_empty() && self.themes.is_empty() {
            return Err(plugin_error("manifest requires entry, grammars, or themes"));
        }
        if !(1..=SUPPORTED_API).contains(&self.api) {
            return Err(plugin_error(format!(
                "plugin {:?} requires API {}, supported API is {}",
                self.name, self.api, SUPPORTED_API
            )));
        }
        if let Some(entry) = &self.entry {
            ensure_inside(root, entry)?;
        }
        for grammar in &self.grammars {
            ensure_inside(root, &grammar.path)?;
        }
        for theme in &self.themes {
            ensure_inside(root, theme.path())?;
        }
        Ok(())
    }
}

impl PluginPackage {
    pub fn load(root: impl AsRef<Path>) -> AppResult<Self> {
        let root = root.as_ref().to_path_buf();
        let manifest_path = root.join("plugin.ttt.json");
        let contents = fs::read_to_string(&manifest_path)?;
        let manifest: Manifest = serde_json::from_str(&contents)
            .map_err(|error| plugin_error(format!("parse {}: {error}", manifest_path.display())))?;
        let mut manifest = manifest;
        let raw: JsonValue = serde_json::from_str(&contents)
            .map_err(|error| plugin_error(format!("parse {}: {error}", manifest_path.display())))?;
        manifest.network_http_hosts = raw
            .get("permissions")
            .and_then(|permissions| permissions.get("network.http"))
            .and_then(JsonValue::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        manifest.validate(&root)?;
        Ok(Self { root, manifest })
    }
    pub fn entry_path(&self) -> Option<PathBuf> {
        self.manifest
            .entry
            .as_ref()
            .map(|entry| self.root.join(entry))
    }
    pub fn theme_paths(&self) -> Vec<PathBuf> {
        self.manifest
            .themes
            .iter()
            .map(|theme| self.root.join(theme.path()))
            .collect()
    }
}

impl PluginRuntime {
    pub fn load(package: &PluginPackage) -> AppResult<Self> {
        let entry = package
            .entry_path()
            .ok_or_else(|| plugin_error("plugin has no Lua entry"))?;
        let source = fs::read_to_string(&entry)?;
        let lua = Lua::new();
        let registration = Rc::new(RefCell::new(PluginRegistration::default()));
        let logs = Rc::new(RefCell::new(Vec::new()));
        let command_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let event_callbacks = Rc::new(RefCell::new(BTreeMap::<String, Vec<RegistryKey>>::new()));
        let panel_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let file_handler_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let file_close_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_file_close_id = Rc::new(RefCell::new(0_u64));
        let terminal_manager = Rc::new(RefCell::new(PluginTerminalManager::default()));
        let (async_tx, async_rx) = mpsc::channel();
        let async_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_async_id = Rc::new(RefCell::new(0_u64));
        let (net_tx, net_rx) = mpsc::channel();
        let net_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_net_id = Rc::new(RefCell::new(0_u64));
        let (spawn_tx, spawn_rx) = mpsc::channel();
        let spawn_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_spawn_id = Rc::new(RefCell::new(0_u64));
        let panel_requests = Rc::new(RefCell::new(Vec::new()));
        let clipboard_text = Rc::new(RefCell::new(String::new()));
        let editor_snapshot = Rc::new(RefCell::new(PluginEditorSnapshot::default()));
        let editor_requests = Rc::new(RefCell::new(Vec::new()));
        let diagnostics = Rc::new(RefCell::new(BTreeMap::new()));
        let bookmarks = Rc::new(RefCell::new(PluginBookmarks::new()));
        let bookmark_events = Rc::new(RefCell::new(Vec::new()));
        let timers = Rc::new(RefCell::new(BTreeMap::new()));
        let next_timer_id = Rc::new(RefCell::new(0_u64));
        let automation_requests = Rc::new(RefCell::new(Vec::new()));
        let next_automation_id = Rc::new(RefCell::new(0_u64));
        let automation_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let dialog_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_dialog_id = Rc::new(RefCell::new(0_u64));
        let command_line_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let command_line_active = Rc::new(RefCell::new(false));
        let custom_dialog_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let dialog_widgets = Rc::new(RefCell::new(BTreeMap::new()));
        let next_custom_dialog_id = Rc::new(RefCell::new(0_u64));
        let (rpc_tx, rpc_rx) = mpsc::channel::<RpcEvent>();
        let rpc_connections = Rc::new(RefCell::new(BTreeMap::new()));
        let rpc_pending = Rc::new(RefCell::new(BTreeMap::new()));
        let next_rpc_id = Rc::new(RefCell::new(0_u64));
        let open_tabs = Rc::new(RefCell::new(BTreeMap::<String, Option<RegistryKey>>::new()));
        let module = lua.create_table().map_err(lua_error)?;

        let confirm_requests = Rc::clone(&panel_requests);
        let confirm_callbacks = Rc::clone(&dialog_callbacks);
        let confirm_ids = Rc::clone(&next_dialog_id);
        module
            .set(
                "confirm",
                lua.create_function(move |lua, (message, callback): (String, Function)| {
                    let id = {
                        let mut next = confirm_ids.borrow_mut();
                        *next = next.saturating_add(1);
                        *next
                    };
                    let key = lua.create_registry_value(callback)?;
                    confirm_callbacks.borrow_mut().insert(id, key);
                    confirm_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::Confirm { id, message });
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let info_requests = Rc::clone(&panel_requests);
        module
            .set(
                "show_info",
                lua.create_function(move |_, (title, entries): (String, Table)| {
                    let mut parsed = Vec::new();
                    for entry in entries.sequence_values::<Table>() {
                        let entry = entry?;
                        parsed.push((entry.get::<String>("key")?, entry.get::<String>("value")?));
                    }
                    info_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::ShowInfo {
                            title,
                            entries: parsed,
                        });
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let custom_dialog_requests = Rc::clone(&panel_requests);
        let custom_dialog_panels = Rc::clone(&panel_callbacks);
        let custom_dialog_callbacks_sink = Rc::clone(&custom_dialog_callbacks);
        let custom_dialog_ids = Rc::clone(&next_custom_dialog_id);
        module
            .set(
                "open_dialog",
                lua.create_function(move |lua, config: Table| {
                    let render = config
                        .get::<Option<Function>>("render")?
                        .ok_or_else(|| mlua::Error::external("open_dialog requires render"))?;
                    let id = {
                        let mut next = custom_dialog_ids.borrow_mut();
                        *next = next.saturating_add(1);
                        *next
                    };
                    let title = config.get::<Option<String>>("title")?.unwrap_or_default();
                    let width = config.get::<Option<u16>>("width")?.unwrap_or(70).max(8);
                    let height = config.get::<Option<u16>>("height")?.unwrap_or(12).max(1);
                    let on_close = config
                        .get::<Option<Function>>("on_close")?
                        .map(|callback| lua.create_registry_value(callback))
                        .transpose()?;
                    custom_dialog_panels
                        .borrow_mut()
                        .insert(format!("__dialog:{id}"), lua.create_registry_value(render)?);
                    custom_dialog_callbacks_sink
                        .borrow_mut()
                        .insert(id, PluginCustomDialogCallbacks { on_close });
                    custom_dialog_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::OpenDialog {
                            id,
                            title,
                            width,
                            height,
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let custom_dialog_requests = Rc::clone(&panel_requests);
        module
            .set(
                "close_dialog",
                lua.create_function(move |_, ()| {
                    custom_dialog_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::CloseDialog);
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let command_line_module = lua.create_table().map_err(lua_error)?;
        let command_line_requests = Rc::clone(&panel_requests);
        let command_line_callbacks_sink = Rc::clone(&command_line_callbacks);
        let command_line_active_sink = Rc::clone(&command_line_active);
        let command_line_ids = Rc::new(RefCell::new(0_u64));
        let command_line_can_use = package
            .manifest
            .permissions
            .get("keybindings")
            .copied()
            .unwrap_or(false);
        command_line_module
            .set(
                "show",
                lua.create_function(move |lua, config: Table| {
                    if !command_line_can_use {
                        return Err(mlua::Error::external("permission denied: keybindings"));
                    }
                    let prefix = config
                        .get::<Option<String>>("prefix")?
                        .unwrap_or_else(|| ":".to_owned());
                    let text = config.get::<Option<String>>("text")?.unwrap_or_default();
                    let id = {
                        let mut next = command_line_ids.borrow_mut();
                        *next = next.saturating_add(1);
                        *next
                    };
                    let callbacks = PluginCommandLineCallbacks {
                        on_change: config
                            .get::<Option<Function>>("on_change")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?,
                        on_submit: config
                            .get::<Option<Function>>("on_submit")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?,
                        on_cancel: config
                            .get::<Option<Function>>("on_cancel")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?,
                        on_key: config
                            .get::<Option<Function>>("on_key")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?,
                    };
                    command_line_callbacks_sink
                        .borrow_mut()
                        .insert(id, callbacks);
                    *command_line_active_sink.borrow_mut() = true;
                    command_line_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::CommandLineShow { id, prefix, text });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let command_line_requests = Rc::clone(&panel_requests);
        let command_line_active_sink = Rc::clone(&command_line_active);
        command_line_module
            .set(
                "hide",
                lua.create_function(move |_, ()| {
                    if !command_line_can_use {
                        return Err(mlua::Error::external("permission denied: keybindings"));
                    }
                    if *command_line_active_sink.borrow() {
                        *command_line_active_sink.borrow_mut() = false;
                        command_line_requests
                            .borrow_mut()
                            .push(PluginPanelRequest::CommandLineHide);
                    }
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let command_line_requests = Rc::clone(&panel_requests);
        let command_line_active_sink = Rc::clone(&command_line_active);
        command_line_module
            .set(
                "set_text",
                lua.create_function(move |_, text: String| {
                    if !command_line_can_use {
                        return Err(mlua::Error::external("permission denied: keybindings"));
                    }
                    if *command_line_active_sink.borrow() {
                        command_line_requests
                            .borrow_mut()
                            .push(PluginPanelRequest::CommandLineSetText { text });
                    }
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let command_line_active_sink = Rc::clone(&command_line_active);
        command_line_module
            .set(
                "active",
                lua.create_function(move |_, ()| Ok(*command_line_active_sink.borrow()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let log_sink = Rc::clone(&logs);
        module
            .set(
                "log",
                lua.create_function(move |_lua, (first, second): (LuaValue, Option<String>)| {
                    let first = match first {
                        LuaValue::String(value) => value
                            .to_str()
                            .map_or_else(|_| String::new(), |value| value.to_owned()),
                        other => format!("{other:?}"),
                    };
                    let (level, message) = match second {
                        Some(message) => (first, message),
                        None => ("info".to_owned(), first),
                    };
                    log_sink.borrow_mut().push(format!("[{level}] {message}"));
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let notify_sink = Rc::clone(&logs);
        module
            .set(
                "notify",
                lua.create_function(move |_lua, (message, level): (String, Option<String>)| {
                    notify_sink.borrow_mut().push(format!(
                        "[{}] {message}",
                        level.unwrap_or_else(|| "info".to_owned())
                    ));
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let status_sink = Rc::clone(&registration);
        let status_can_write = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "set_status_item",
                lua.create_function(
                    move |_, (side, id, text, options): (String, String, String, Option<Table>)| {
                        if !status_can_write {
                            return Err(mlua::Error::external("permission denied: commands"));
                        }
                        if !matches!(side.as_str(), "left" | "right") {
                            return Err(mlua::Error::external(
                                "status item side must be left or right",
                            ));
                        }
                        let priority = options
                            .and_then(|options| {
                                options.get::<Option<i64>>("priority").ok().flatten()
                            })
                            .unwrap_or(1000);
                        let mut registration = status_sink.borrow_mut();
                        registration.status_items.retain(|item| item.id != id);
                        registration.status_items.push(PluginStatusItem {
                            id,
                            text,
                            side,
                            priority,
                        });
                        Ok(())
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let status_sink = Rc::clone(&registration);
        let status_can_write = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "remove_status_item",
                lua.create_function(move |_, id: String| {
                    if !status_can_write {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    status_sink
                        .borrow_mut()
                        .status_items
                        .retain(|item| item.id != id);
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let command_sink = Rc::clone(&command_callbacks);
        let command_can_execute = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "exec_command",
                lua.create_function(move |lua, id: String| {
                    if !command_can_execute {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    let callback = {
                        let callbacks = command_sink.borrow();
                        let Some(key) = callbacks.get(&id) else {
                            return Ok(false);
                        };
                        lua.registry_value::<Function>(key)
                            .map_err(|error| mlua::Error::external(error.to_string()))?
                    };
                    callback.call::<()>(()).map_err(|error| {
                        mlua::Error::external(format!("command {id} failed: {error}"))
                    })?;
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let command_list = Rc::clone(&registration);
        let command_can_list = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "list_commands",
                lua.create_function(move |lua, ()| {
                    if !command_can_list {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    let result = lua.create_table()?;
                    for (index, command) in command_list.borrow().commands.iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("id", command.id.as_str())?;
                        item.set("title", command.title.as_str())?;
                        result.set(index + 1, item)?;
                    }
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let panel_registration = Rc::clone(&registration);
        let panel_request_sink = Rc::clone(&panel_requests);
        let can_show_sidebar = package
            .manifest
            .permissions
            .get("panel.sidebar")
            .copied()
            .unwrap_or(false);
        let can_show_bottom = package
            .manifest
            .permissions
            .get("panel.bottom")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "show_panel",
                lua.create_function(move |_, area: String| {
                    let location = match area.as_str() {
                        "sidebar" if can_show_sidebar => PluginPanelLocation::Sidebar,
                        "bottom" if can_show_bottom => PluginPanelLocation::Bottom,
                        "sidebar" => {
                            return Err(mlua::Error::external("permission denied: panel.sidebar"));
                        }
                        "bottom" => {
                            return Err(mlua::Error::external("permission denied: panel.bottom"));
                        }
                        _ => return Err(mlua::Error::external("unknown panel area")),
                    };
                    if !panel_registration
                        .borrow()
                        .panels
                        .iter()
                        .any(|panel| panel.location == location)
                    {
                        return Err(mlua::Error::external(format!("plugin has no {area} panel")));
                    }
                    panel_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::Show {
                            location,
                            focus: false,
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let panel_registration = Rc::clone(&registration);
        let panel_request_sink = Rc::clone(&panel_requests);
        module
            .set(
                "focus_panel",
                lua.create_function(move |_, area: String| {
                    let location = match area.as_str() {
                        "sidebar" if can_show_sidebar => PluginPanelLocation::Sidebar,
                        "bottom" if can_show_bottom => PluginPanelLocation::Bottom,
                        "sidebar" => {
                            return Err(mlua::Error::external("permission denied: panel.sidebar"));
                        }
                        "bottom" => {
                            return Err(mlua::Error::external("permission denied: panel.bottom"));
                        }
                        _ => return Err(mlua::Error::external("unknown panel area")),
                    };
                    if !panel_registration
                        .borrow()
                        .panels
                        .iter()
                        .any(|panel| panel.location == location)
                    {
                        return Err(mlua::Error::external(format!("plugin has no {area} panel")));
                    }
                    panel_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::Show {
                            location,
                            focus: true,
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let drawer_panel_sink = Rc::clone(&panel_callbacks);
        let drawer_request_sink = Rc::clone(&panel_requests);
        let can_drawer = package
            .manifest
            .permissions
            .get("panel.drawer")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "open_drawer",
                lua.create_function(move |lua, config: Table| {
                    if !can_drawer {
                        return Err(mlua::Error::external("permission denied: panel.drawer"));
                    }
                    let side = config
                        .get::<Option<String>>("side")?
                        .unwrap_or_else(|| "right".to_owned());
                    if !matches!(side.as_str(), "left" | "right") {
                        return Err(mlua::Error::external("drawer side must be left or right"));
                    }
                    let width = config.get::<Option<u16>>("width")?.unwrap_or(40).max(1);
                    let min_width = config
                        .get::<Option<u16>>("min_width")?
                        .unwrap_or(20)
                        .max(1)
                        .min(width);
                    let render = config
                        .get::<Option<Function>>("render")?
                        .ok_or_else(|| mlua::Error::external("drawer render is required"))?;
                    drawer_panel_sink
                        .borrow_mut()
                        .insert("__drawer".to_owned(), lua.create_registry_value(render)?);
                    drawer_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::OpenDrawer {
                            side,
                            width,
                            min_width,
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let drawer_request_sink = Rc::clone(&panel_requests);
        let can_drawer = package
            .manifest
            .permissions
            .get("panel.drawer")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "close_drawer",
                lua.create_function(move |_, ()| {
                    if !can_drawer {
                        return Err(mlua::Error::external("permission denied: panel.drawer"));
                    }
                    drawer_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::CloseDrawer);
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let tab_panel_sink = Rc::clone(&panel_callbacks);
        let tab_request_sink = Rc::clone(&panel_requests);
        let open_tabs_sink = Rc::clone(&open_tabs);
        let can_editor_panel = package
            .manifest
            .permissions
            .get("panel.editor")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "open_tab",
                lua.create_function(move |lua, config: Table| {
                    if !can_editor_panel {
                        return Err(mlua::Error::external("permission denied: panel.editor"));
                    }
                    let title = config
                        .get::<Option<String>>("title")?
                        .filter(|title| !title.is_empty())
                        .ok_or_else(|| mlua::Error::external("tab title is required"))?;
                    let id = config
                        .get::<Option<String>>("id")?
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| title.clone());
                    let render = config
                        .get::<Option<Function>>("render")?
                        .ok_or_else(|| mlua::Error::external("tab render is required"))?;
                    let on_close = config
                        .get::<Option<Function>>("on_close")?
                        .map(|callback| lua.create_registry_value(callback))
                        .transpose()?;
                    let focus = config.get::<Option<bool>>("focus")?.unwrap_or(true);
                    tab_panel_sink
                        .borrow_mut()
                        .insert(format!("__tab:{id}"), lua.create_registry_value(render)?);
                    let tab_id = id.clone();
                    open_tabs_sink.borrow_mut().insert(id.clone(), on_close);
                    tab_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::OpenTab { id, title, focus });
                    Ok(tab_id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let tab_request_sink = Rc::clone(&panel_requests);
        let open_tabs_sink = Rc::clone(&open_tabs);
        let can_editor_panel = package
            .manifest
            .permissions
            .get("panel.editor")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "close_tab",
                lua.create_function(move |_, id: String| {
                    if !can_editor_panel {
                        return Err(mlua::Error::external("permission denied: panel.editor"));
                    }
                    if !open_tabs_sink.borrow().contains_key(&id) {
                        return Ok(false);
                    }
                    tab_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::CloseTab { id });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let tab_request_sink = Rc::clone(&panel_requests);
        let open_tabs_sink = Rc::clone(&open_tabs);
        let can_editor_panel = package
            .manifest
            .permissions
            .get("panel.editor")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "focus_tab",
                lua.create_function(move |_, id: String| {
                    if !can_editor_panel {
                        return Err(mlua::Error::external("permission denied: panel.editor"));
                    }
                    if !open_tabs_sink.borrow().contains_key(&id) {
                        return Ok(false);
                    }
                    tab_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::FocusTab { id });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let readonly_panel_sink = Rc::clone(&panel_callbacks);
        let readonly_request_sink = Rc::clone(&panel_requests);
        let readonly_tabs_sink = Rc::clone(&open_tabs);
        module
            .set(
                "open_readonly",
                lua.create_function(
                    move |lua, (title, lines, _file_path): (String, Table, Option<String>)| {
                        if title.is_empty() {
                            return Err(mlua::Error::external("readonly tab title is required"));
                        }
                        let lines = lines
                            .sequence_values::<String>()
                            .collect::<mlua::Result<Vec<_>>>()?;
                        let render = lua.create_function(move |lua, panel: Table| {
                            let label = panel.get::<Function>("label")?;
                            for line in &lines {
                                label.call::<()>((
                                    panel.clone(),
                                    Some(LuaValue::String(lua.create_string(line)?)),
                                ))?;
                            }
                            Ok(())
                        })?;
                        let id = title.clone();
                        readonly_panel_sink
                            .borrow_mut()
                            .insert(format!("__tab:{id}"), lua.create_registry_value(render)?);
                        readonly_tabs_sink.borrow_mut().insert(id.clone(), None);
                        readonly_request_sink
                            .borrow_mut()
                            .push(PluginPanelRequest::OpenTab {
                                id: id.clone(),
                                title,
                                focus: true,
                            });
                        Ok(id)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let diff_panel_sink = Rc::clone(&panel_callbacks);
        let diff_request_sink = Rc::clone(&panel_requests);
        let diff_tabs_sink = Rc::clone(&open_tabs);
        module
            .set(
                "open_diff",
                lua.create_function(
                    move |lua,
                          (title, old_lines, new_lines, _file_path): (
                        String,
                        Table,
                        Table,
                        Option<String>,
                    )| {
                        if title.is_empty() {
                            return Err(mlua::Error::external("diff tab title is required"));
                        }
                        let old_lines = old_lines
                            .sequence_values::<String>()
                            .collect::<mlua::Result<Vec<_>>>()?;
                        let new_lines = new_lines
                            .sequence_values::<String>()
                            .collect::<mlua::Result<Vec<_>>>()?;
                        let diff_lines = line_diff(&old_lines, &new_lines);
                        let render = lua.create_function(move |lua, panel: Table| {
                            let label = panel.get::<Function>("label")?;
                            for line in &diff_lines {
                                label.call::<()>((
                                    panel.clone(),
                                    Some(LuaValue::String(lua.create_string(line)?)),
                                ))?;
                            }
                            Ok(())
                        })?;
                        let id = title.clone();
                        diff_panel_sink
                            .borrow_mut()
                            .insert(format!("__tab:{id}"), lua.create_registry_value(render)?);
                        diff_tabs_sink.borrow_mut().insert(id.clone(), None);
                        diff_request_sink
                            .borrow_mut()
                            .push(PluginPanelRequest::OpenTab {
                                id: id.clone(),
                                title,
                                focus: true,
                            });
                        Ok(id)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let changes_panel_sink = Rc::clone(&panel_callbacks);
        let changes_request_sink = Rc::clone(&panel_requests);
        let changes_tabs_sink = Rc::clone(&open_tabs);
        module
            .set(
                "open_changes",
                lua.create_function(move |lua, config: Table| {
                    let diff = config
                        .get::<Option<String>>("diff")?
                        .ok_or_else(|| mlua::Error::external("changes diff is required"))?;
                    if diff.trim().is_empty() {
                        return Err(mlua::Error::external("changes diff must not be empty"));
                    }
                    if !diff.lines().any(|line| line.starts_with("diff --git ")) {
                        return Err(mlua::Error::external(
                            "changes diff must contain diff --git headers",
                        ));
                    }
                    let title = config
                        .get::<Option<String>>("title")?
                        .unwrap_or_else(|| "Changes".to_owned());
                    let id = config
                        .get::<Option<String>>("id")?
                        .unwrap_or_else(|| title.clone());
                    if id.is_empty() {
                        return Err(mlua::Error::external("changes tab id must not be empty"));
                    }
                    let description = config
                        .get::<Option<String>>("description")?
                        .unwrap_or_default();
                    let lines = diff.lines().map(str::to_owned).collect::<Vec<_>>();
                    let render = lua.create_function(move |lua, panel: Table| {
                        let label = panel.get::<Function>("label")?;
                        if !description.is_empty() {
                            label.call::<()>((
                                panel.clone(),
                                Some(LuaValue::String(lua.create_string(&description)?)),
                            ))?;
                        }
                        for line in &lines {
                            label.call::<()>((
                                panel.clone(),
                                Some(LuaValue::String(lua.create_string(line)?)),
                            ))?;
                        }
                        Ok(())
                    })?;
                    changes_panel_sink
                        .borrow_mut()
                        .insert(format!("__tab:{id}"), lua.create_registry_value(render)?);
                    changes_tabs_sink.borrow_mut().insert(id.clone(), None);
                    changes_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::OpenTab {
                            id: id.clone(),
                            title,
                            focus: true,
                        });
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let file_request_sink = Rc::clone(&panel_requests);
        module
            .set(
                "open_file",
                lua.create_function(
                    move |_, (path, line, readonly): (String, Option<usize>, Option<bool>)| {
                        if path.trim().is_empty() {
                            return Err(mlua::Error::external("file path is required"));
                        }
                        file_request_sink
                            .borrow_mut()
                            .push(PluginPanelRequest::OpenFile {
                                path,
                                line,
                                readonly: readonly.unwrap_or(false),
                            });
                        Ok(true)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let clipboard_request_sink = Rc::clone(&panel_requests);
        let clipboard_text_sink = Rc::clone(&clipboard_text);
        let can_clipboard_write = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "clipboard_write",
                lua.create_function(move |_, text: String| {
                    if !can_clipboard_write {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    *clipboard_text_sink.borrow_mut() = text.clone();
                    clipboard_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::ClipboardWrite { text });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let clipboard_text_source = Rc::clone(&clipboard_text);
        let can_clipboard_read = package
            .manifest
            .permissions
            .get("clipboard.read")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "clipboard_read",
                lua.create_function(move |_, ()| {
                    if !can_clipboard_read {
                        return Err(mlua::Error::external("permission denied: clipboard.read"));
                    }
                    Ok(clipboard_text_source.borrow().clone())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let register_sink = Rc::clone(&registration);
        let callback_sink = Rc::clone(&command_callbacks);
        let panel_sink = Rc::clone(&panel_callbacks);
        let file_handler_sink = Rc::clone(&file_handler_callbacks);
        let can_commands = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        let can_keybindings = package
            .manifest
            .permissions
            .get("keybindings")
            .copied()
            .unwrap_or(false);
        let can_sidebar = package
            .manifest
            .permissions
            .get("panel.sidebar")
            .copied()
            .unwrap_or(false);
        let can_bottom = package
            .manifest
            .permissions
            .get("panel.bottom")
            .copied()
            .unwrap_or(false);
        let can_file_handlers = package
            .manifest
            .permissions
            .get("editor.file_handlers")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "register",
                lua.create_function(move |lua, config: Table| {
                    if config.get::<Option<Table>>("commands")?.is_some() && !can_commands {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    if config.get::<Option<Table>>("keybindings")?.is_some() && !can_keybindings {
                        return Err(mlua::Error::external("permission denied: keybindings"));
                    }
                    if config.get::<Option<Table>>("sidebar")?.is_some() && !can_sidebar {
                        return Err(mlua::Error::external("permission denied: panel.sidebar"));
                    }
                    if config.get::<Option<Table>>("bottom")?.is_some() && !can_bottom {
                        return Err(mlua::Error::external("permission denied: panel.bottom"));
                    }
                    if config.get::<Option<Table>>("file_handlers")?.is_some() && !can_file_handlers
                    {
                        return Err(mlua::Error::external(
                            "permission denied: editor.file_handlers",
                        ));
                    }
                    let mut registered = register_sink.borrow_mut();
                    registered.panels.clear();
                    registered.sidebar = config.get::<Option<Table>>("sidebar")?.is_some();
                    registered.bottom = config.get::<Option<Table>>("bottom")?.is_some();
                    registered.commands = config
                        .get::<Option<Table>>("commands")?
                        .map_or(Ok(Vec::new()), parse_commands)?;
                    if let Some(commands) = config.get::<Option<Table>>("commands")? {
                        for item in commands.sequence_values::<Table>() {
                            let item = item?;
                            let Some(id) = item.get::<Option<String>>("id")? else {
                                continue;
                            };
                            if let Some(callback) = item
                                .get::<Option<Function>>("handler")?
                                .or(item.get::<Option<Function>>("run")?)
                            {
                                let key = lua.create_registry_value(callback)?;
                                callback_sink.borrow_mut().insert(id, key);
                            }
                        }
                    }
                    registered.keybindings = config
                        .get::<Option<Table>>("keybindings")?
                        .map_or(Ok(Vec::new()), parse_keybindings)?;
                    registered.status_items = config
                        .get::<Option<Table>>("status_items")?
                        .or(config.get::<Option<Table>>("statusItems")?)
                        .map_or(Ok(Vec::new()), parse_status_items)?;
                    registered.file_handlers = config
                        .get::<Option<Table>>("file_handlers")?
                        .map_or(Ok(Vec::new()), parse_file_handlers)?;
                    if let Some(handlers) = config.get::<Option<Table>>("file_handlers")? {
                        for item in handlers.sequence_values::<Table>() {
                            let item = item?;
                            let Some(id) = item.get::<Option<String>>("id")? else {
                                continue;
                            };
                            if let Some(open) = item.get::<Option<Function>>("open")? {
                                file_handler_sink
                                    .borrow_mut()
                                    .insert(id, lua.create_registry_value(open)?);
                            }
                        }
                    }
                    if let Some(sidebar) = config.get::<Option<Table>>("sidebar")? {
                        if let Some(panel) = parse_panel(&sidebar, PluginPanelLocation::Sidebar)? {
                            let id = panel.id.clone();
                            registered.panels.push(panel);
                            if let Some(render) = sidebar.get::<Option<Function>>("render")? {
                                panel_sink
                                    .borrow_mut()
                                    .insert(id, lua.create_registry_value(render)?);
                            }
                        }
                    }
                    if let Some(bottom) = config.get::<Option<Table>>("bottom")? {
                        if let Some(panel) = parse_panel(&bottom, PluginPanelLocation::Bottom)? {
                            let id = panel.id.clone();
                            registered.panels.push(panel);
                            if let Some(render) = bottom.get::<Option<Function>>("render")? {
                                panel_sink
                                    .borrow_mut()
                                    .insert(id, lua.create_registry_value(render)?);
                            }
                        }
                    }
                    let _ = lua; // keep the callback signature explicit for future API wiring
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let event_sink = Rc::clone(&event_callbacks);
        module
            .set(
                "on",
                lua.create_function(move |lua, (event, callback): (String, Function)| {
                    let key = lua.create_registry_value(callback)?;
                    event_sink.borrow_mut().entry(event).or_default().push(key);
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let events_module = lua.create_table().map_err(lua_error)?;
        let event_sink = Rc::clone(&event_callbacks);
        events_module
            .set(
                "on",
                lua.create_function(move |lua, (event, callback): (String, Function)| {
                    let key = lua.create_registry_value(callback)?;
                    event_sink.borrow_mut().entry(event).or_default().push(key);
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        module
            .set(
                "version",
                lua.create_function(|_, ()| Ok(env!("CARGO_PKG_VERSION")))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        module
            .set(
                "platform",
                lua.create_function(|_, ()| Ok(std::env::consts::OS))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        module
            .set(
                "arch",
                lua.create_function(|_, ()| Ok(std::env::consts::ARCH))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let plugin_dir = package.root.to_string_lossy().to_string();
        module
            .set(
                "plugin_dir",
                lua.create_function(move |_, ()| Ok(plugin_dir.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        module
            .set(
                "markdown",
                lua.create_function(|lua, text: String| markdown_to_lua(lua, &text))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        module
            .set(
                "executable_path",
                lua.create_function(|_, ()| {
                    std::env::current_exe()
                        .map(|path| path.to_string_lossy().into_owned())
                        .map_err(mlua::Error::external)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        module
            .set(
                "graphics",
                lua.create_function(|lua, ()| {
                    let result = lua.create_table()?;
                    result.set("protocol", "none")?;
                    result.set("cell_w", 0_u16)?;
                    result.set("cell_h", 0_u16)?;
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let screen_snapshot = Rc::clone(&editor_snapshot);
        let can_screen_read = package
            .manifest
            .permissions
            .get("screen.read")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "screenshot",
                lua.create_function(move |_, _path: Option<String>| {
                    if !can_screen_read {
                        return Err(mlua::Error::external("permission denied: screen.read"));
                    }
                    Ok(screen_snapshot.borrow().lines.join("\n"))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let debug_snapshot = Rc::clone(&editor_snapshot);
        module
            .set(
                "debug",
                lua.create_function(move |_, _path: Option<String>| {
                    if !can_screen_read {
                        return Err(mlua::Error::external("permission denied: screen.read"));
                    }
                    let snapshot = debug_snapshot.borrow();
                    serde_json::to_string(&json!({
                        "path": snapshot.path,
                        "file_name": snapshot.file_name,
                        "language": snapshot.language,
                        "cursor": {"line": snapshot.cursor_line + 1, "col": snapshot.cursor_col + 1},
                        "buffer_lines": snapshot.lines.len(),
                        "selection": snapshot.selection,
                        "cursors": snapshot.cursors,
                    }))
                    .map_err(mlua::Error::external)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let timer_sink = Rc::clone(&timers);
        let timer_ids = Rc::clone(&next_timer_id);
        module
            .set(
                "set_timeout",
                lua.create_function(move |lua, (milliseconds, callback): (u64, Function)| {
                    let id = {
                        let mut next = timer_ids.borrow_mut();
                        *next += 1;
                        *next
                    };
                    let callback = lua.create_registry_value(callback)?;
                    timer_sink.borrow_mut().insert(
                        id,
                        PluginTimer {
                            callback,
                            next: Instant::now()
                                + Duration::from_millis(milliseconds.min(86_400_000)),
                            interval: None,
                        },
                    );
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let timer_sink = Rc::clone(&timers);
        let timer_ids = Rc::clone(&next_timer_id);
        module
            .set(
                "set_interval",
                lua.create_function(move |lua, (milliseconds, callback): (u64, Function)| {
                    let id = {
                        let mut next = timer_ids.borrow_mut();
                        *next += 1;
                        *next
                    };
                    let callback = lua.create_registry_value(callback)?;
                    let interval = Duration::from_millis(milliseconds.clamp(1, 86_400_000));
                    timer_sink.borrow_mut().insert(
                        id,
                        PluginTimer {
                            callback,
                            next: Instant::now() + interval,
                            interval: Some(interval),
                        },
                    );
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let timer_sink = Rc::clone(&timers);
        for name in ["clear_timeout", "clear_interval"] {
            let timer_sink = Rc::clone(&timer_sink);
            module
                .set(
                    name,
                    lua.create_function(move |_, id: u64| {
                        Ok(timer_sink.borrow_mut().remove(&id).is_some())
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let debounce_timers = Rc::clone(&timers);
        let debounce_ids = Rc::clone(&next_timer_id);
        module
            .set(
                "debounce",
                lua.create_function(move |lua, (milliseconds, callback): (u64, Function)| {
                    let callback_key = Rc::new(lua.create_registry_value(callback)?);
                    let state = Rc::new(RefCell::new((None::<u64>, Vec::<LuaValue>::new())));
                    let run_state = Rc::clone(&state);
                    let run_timers = Rc::clone(&debounce_timers);
                    let run_ids = Rc::clone(&debounce_ids);
                    let run_callback = Rc::clone(&callback_key);
                    let run = lua.create_function(move |lua, args: Variadic<LuaValue>| {
                        if let Some(previous) = run_state.borrow_mut().0.take() {
                            run_timers.borrow_mut().remove(&previous);
                        }
                        run_state.borrow_mut().1 = args.into_iter().collect();
                        let id = {
                            let mut next = run_ids.borrow_mut();
                            *next = next.saturating_add(1);
                            *next
                        };
                        let callback_state = Rc::clone(&run_state);
                        let callback_key = Rc::clone(&run_callback);
                        let timer_callback = lua.create_function(move |lua, ()| {
                            let args = {
                                let mut state = callback_state.borrow_mut();
                                state.0 = None;
                                std::mem::take(&mut state.1)
                            };
                            let callback: Function = lua.registry_value(&callback_key)?;
                            callback.call::<()>(MultiValue::from_vec(args))
                        })?;
                        let timer_key = lua.create_registry_value(timer_callback)?;
                        run_timers.borrow_mut().insert(
                            id,
                            PluginTimer {
                                callback: timer_key,
                                next: Instant::now()
                                    + Duration::from_millis(milliseconds.min(86_400_000)),
                                interval: None,
                            },
                        );
                        run_state.borrow_mut().0 = Some(id);
                        Ok(())
                    })?;
                    let cancel_state = Rc::clone(&state);
                    let cancel_timers = Rc::clone(&debounce_timers);
                    let cancel = lua.create_function(move |_, ()| {
                        let Some(id) = cancel_state.borrow_mut().0.take() else {
                            return Ok(false);
                        };
                        Ok(cancel_timers.borrow_mut().remove(&id).is_some())
                    })?;
                    Ok((run, cancel))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let automation_module = lua.create_table().map_err(lua_error)?;
        let automation_sink = Rc::clone(&automation_requests);
        let automation_ids = Rc::clone(&next_automation_id);
        automation_module
            .set(
                "run",
                lua.create_function(
                    move |lua, (action, args, callback): (String, String, Function)| {
                        let id = {
                            let mut next = automation_ids.borrow_mut();
                            *next += 1;
                            *next
                        };
                        let callback = lua.create_registry_value(callback)?;
                        automation_sink.borrow_mut().push(PluginAutomationRequest {
                            task: PluginAutomationTask { id, action, args },
                            callback,
                        });
                        let cancel_sink = Rc::clone(&automation_sink);
                        lua.create_function(move |_, ()| {
                            let before = cancel_sink.borrow().len();
                            cancel_sink
                                .borrow_mut()
                                .retain(|request| request.task.id != id);
                            Ok(cancel_sink.borrow().len() != before)
                        })
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let can_input = package
            .manifest
            .permissions
            .get("input")
            .copied()
            .unwrap_or(false);
        for (name, action) in [("key", "key"), ("type", "type")] {
            let input_sink = Rc::clone(&automation_requests);
            let input_ids = Rc::clone(&next_automation_id);
            module
                .set(
                    name,
                    lua.create_function(move |lua, args: String| {
                        if !can_input {
                            return Err(mlua::Error::external("permission denied: input"));
                        }
                        let id = {
                            let mut next = input_ids.borrow_mut();
                            *next += 1;
                            *next
                        };
                        let callback = lua.create_function(
                            |_, (_result, _error): (LuaValue, Option<String>)| Ok(()),
                        )?;
                        input_sink.borrow_mut().push(PluginAutomationRequest {
                            task: PluginAutomationTask {
                                id,
                                action: action.to_owned(),
                                args,
                            },
                            callback: lua.create_registry_value(callback)?,
                        });
                        Ok(true)
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let click_sink = Rc::clone(&automation_requests);
        let click_ids = Rc::clone(&next_automation_id);
        module
            .set(
                "click",
                lua.create_function(move |lua, (x, y): (u16, u16)| {
                    if !can_input {
                        return Err(mlua::Error::external("permission denied: input"));
                    }
                    let id = {
                        let mut next = click_ids.borrow_mut();
                        *next += 1;
                        *next
                    };
                    let callback =
                        lua.create_function(|_, (_result, _error): (LuaValue, Option<String>)| {
                            Ok(())
                        })?;
                    click_sink.borrow_mut().push(PluginAutomationRequest {
                        task: PluginAutomationTask {
                            id,
                            action: "click".to_owned(),
                            args: format!("{x} {y}"),
                        },
                        callback: lua.create_registry_value(callback)?,
                    });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let drag_sink = Rc::clone(&automation_requests);
        let drag_ids = Rc::clone(&next_automation_id);
        module
            .set(
                "drag",
                lua.create_function(move |lua, (x1, y1, x2, y2): (u16, u16, u16, u16)| {
                    if !can_input {
                        return Err(mlua::Error::external("permission denied: input"));
                    }
                    let id = {
                        let mut next = drag_ids.borrow_mut();
                        *next += 1;
                        *next
                    };
                    let callback =
                        lua.create_function(|_, (_result, _error): (LuaValue, Option<String>)| {
                            Ok(())
                        })?;
                    drag_sink.borrow_mut().push(PluginAutomationRequest {
                        task: PluginAutomationTask {
                            id,
                            action: "drag".to_owned(),
                            args: format!("{x1} {y1} {x2} {y2}"),
                        },
                        callback: lua.create_registry_value(callback)?,
                    });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let can_quit = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        let quit_sink = Rc::clone(&automation_requests);
        let quit_ids = Rc::clone(&next_automation_id);
        module
            .set(
                "quit",
                lua.create_function(move |lua, ()| {
                    if !can_quit {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    let id = {
                        let mut next = quit_ids.borrow_mut();
                        *next += 1;
                        *next
                    };
                    let callback =
                        lua.create_function(|_, (_result, _error): (LuaValue, Option<String>)| {
                            Ok(())
                        })?;
                    quit_sink.borrow_mut().push(PluginAutomationRequest {
                        task: PluginAutomationTask {
                            id,
                            action: "quit".to_owned(),
                            args: String::new(),
                        },
                        callback: lua.create_registry_value(callback)?,
                    });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let rpc_module = lua.create_table().map_err(lua_error)?;
        let can_rpc = package
            .manifest
            .permissions
            .get("network.rpc")
            .copied()
            .unwrap_or(false);
        let rpc_connections_for_lua = Rc::clone(&rpc_connections);
        let rpc_pending_for_lua = Rc::clone(&rpc_pending);
        rpc_module
            .set(
                "connect",
                lua.create_function(
                    move |lua, (endpoint, options, on_connect): (String, Table, Function)| {
                        if !can_rpc {
                            return Err(mlua::Error::external("permission denied: network.rpc"));
                        }
                        let stream =
                            connect_rpc_transport(&endpoint).map_err(mlua::Error::external)?;
                        let reader = stream.try_clone().map_err(mlua::Error::external)?;
                        let writer = Arc::new(Mutex::new(stream));
                        let connection_id = {
                            let mut next = next_rpc_id.borrow_mut();
                            *next += 1;
                            *next
                        };
                        let on_notification = options
                            .get::<Option<Function>>("on_notification")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?;
                        let on_request = options
                            .get::<Option<Function>>("on_request")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?;
                        let on_close = options
                            .get::<Option<Function>>("on_close")?
                            .map(|callback| lua.create_registry_value(callback))
                            .transpose()?;
                        rpc_connections_for_lua.borrow_mut().insert(
                            connection_id,
                            RpcRegistration {
                                writer: Arc::clone(&writer),
                                on_notification,
                                on_request,
                                on_close,
                            },
                        );
                        spawn_rpc_reader(connection_id, reader, rpc_tx.clone());
                        let closed = Rc::new(RefCell::new(false));
                        let next_id = Rc::new(RefCell::new(0_u64));
                        let connection = lua.create_table()?;

                        let notify_stream = Arc::clone(&writer);
                        let notify_closed = Rc::clone(&closed);
                        connection.set(
                            "notify",
                            lua.create_function(
                                move |lua, (_self, method, params): (Table, String, LuaValue)| {
                                    if *notify_closed.borrow() {
                                        return Err(mlua::Error::external("RPC connection closed"));
                                    }
                                    let params: JsonValue = lua.from_value(params)?;
                                    let mut stream = notify_stream.lock().map_err(|_| {
                                        mlua::Error::external("RPC writer poisoned")
                                    })?;
                                    write_rpc_message(
                                        &mut stream,
                                        &json!({"jsonrpc":"2.0","method":method,"params":params}),
                                    )
                                    .map_err(mlua::Error::external)?;
                                    Ok(true)
                                },
                            )?,
                        )?;

                        let request_stream = Arc::clone(&writer);
                        let request_closed = Rc::clone(&closed);
                        let request_ids = Rc::clone(&next_id);
                        let request_pending = Rc::clone(&rpc_pending_for_lua);
                        connection.set(
                            "request",
                            lua.create_function(
                                move |lua,
                                      (_self, method, params, callback): (
                                    Table,
                                    String,
                                    LuaValue,
                                    Function,
                                )| {
                                    if *request_closed.borrow() {
                                        return Err(mlua::Error::external("RPC connection closed"));
                                    }
                                    let id = {
                                        let mut next = request_ids.borrow_mut();
                                        *next += 1;
                                        *next
                                    };
                                    let params: JsonValue = lua.from_value(params)?;
                                    request_pending.borrow_mut().insert(
                                        (connection_id, id),
                                        lua.create_registry_value(callback)?,
                                    );
                                    let mut stream = request_stream.lock().map_err(|_| {
                                        mlua::Error::external("RPC writer poisoned")
                                    })?;
                                    if let Err(error) = write_rpc_message(
                                        &mut stream,
                                        &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                                    ) {
                                        request_pending.borrow_mut().remove(&(connection_id, id));
                                        return Err(mlua::Error::external(error));
                                    }
                                    let cancel_stream = Arc::clone(&request_stream);
                                    let cancel_closed = Rc::clone(&request_closed);
                                    let cancel_pending = Rc::clone(&request_pending);
                                    let cancel = lua.create_function(move |_, ()| {
                                        if *cancel_closed.borrow() {
                                            return Ok(false);
                                        }
                                        if cancel_pending
                                            .borrow_mut()
                                            .remove(&(connection_id, id))
                                            .is_none()
                                        {
                                            return Ok(false);
                                        }
                                        let mut stream = cancel_stream.lock().map_err(|_| {
                                            mlua::Error::external("RPC writer poisoned")
                                        })?;
                                        write_rpc_message(
                                            &mut stream,
                                            &json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}),
                                        )
                                        .map_err(mlua::Error::external)?;
                                        Ok(true)
                                    })?;
                                    Ok(cancel)
                                },
                            )?,
                        )?;

                        let close_closed = Rc::clone(&closed);
                        let close_stream = Arc::clone(&writer);
                        connection.set(
                            "close",
                            lua.create_function(move |_, _self: Table| {
                                *close_closed.borrow_mut() = true;
                                if let Ok(stream) = close_stream.lock() {
                                    stream
                                        .shutdown()
                                        .map_err(mlua::Error::external)?;
                                }
                                Ok(true)
                            })?,
                        )?;
                        on_connect.call::<()>((connection, LuaValue::Nil))?;
                        Ok(true)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let json_module = lua.create_table().map_err(lua_error)?;
        json_module
            .set(
                "encode",
                lua.create_function(|lua, value: LuaValue| {
                    let value: JsonValue = lua.from_value(value).map_err(mlua::Error::external)?;
                    serde_json::to_string(&value).map_err(mlua::Error::external)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        json_module
            .set(
                "decode",
                lua.create_function(|lua, text: String| {
                    let value: JsonValue =
                        serde_json::from_str(&text).map_err(mlua::Error::external)?;
                    lua.to_value(&value)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let system_module = lua.create_table().map_err(lua_error)?;
        let can_read_env = package
            .manifest
            .permissions
            .get("system.env")
            .copied()
            .unwrap_or(false);
        system_module
            .set(
                "env",
                lua.create_function(move |_, name: String| {
                    if !can_read_env {
                        return Err(mlua::Error::external("permission denied: system.env"));
                    }
                    Ok(std::env::var(name).unwrap_or_default())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let can_exec = package
            .manifest
            .permissions
            .get("system.exec")
            .copied()
            .unwrap_or(false);
        let exec_permission = can_exec;
        system_module
            .set(
                "exec",
                lua.create_function(move |lua, values: Variadic<LuaValue>| {
                    if !exec_permission {
                        return Err(mlua::Error::external("permission denied: system.exec"));
                    }
                    let (argv, options) = parse_command_values(values.to_vec())?;
                    let (output, timed_out) = run_command(&argv, &options)?;
                    command_result(lua, output, timed_out)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let async_permission = can_exec;
        let async_sender = async_tx.clone();
        let async_callback_sink = Rc::clone(&async_callbacks);
        let async_id_source = Rc::clone(&next_async_id);
        system_module
            .set(
                "exec_async",
                lua.create_function(move |lua, values: Variadic<LuaValue>| {
                    if !async_permission {
                        return Err(mlua::Error::external("permission denied: system.exec"));
                    }
                    let mut values = values.into_iter().collect::<Vec<_>>();
                    let callback = match values.pop() {
                        Some(LuaValue::Function(callback)) => callback,
                        _ => return Err(mlua::Error::external("exec_async needs callback")),
                    };
                    let (argv, options) = parse_command_values(values)?;
                    let id = {
                        let mut next = async_id_source.borrow_mut();
                        *next = (*next).saturating_add(1);
                        *next
                    };
                    let key = lua.create_registry_value(callback)?;
                    async_callback_sink.borrow_mut().insert(id, key);
                    let sender = async_sender.clone();
                    std::thread::spawn(move || {
                        let result =
                            run_command(&argv, &options).map_err(|error| error.to_string());
                        let _ = sender.send(AsyncCompletion { id, result });
                    });
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let spawn_allowed = can_exec;
        let spawn_events = spawn_tx.clone();
        let spawn_callbacks_sink = Rc::clone(&spawn_callbacks);
        let spawn_ids = Rc::clone(&next_spawn_id);
        system_module
            .set(
                "spawn",
                lua.create_function(move |lua, options: Table| {
                    if !spawn_allowed {
                        return Err(mlua::Error::external("permission denied: system.exec"));
                    }
                    let shell = options
                        .get::<Option<String>>("shell")?
                        .unwrap_or_else(|| "sh".to_owned());
                    if shell.is_empty() {
                        return Err(mlua::Error::external("spawn shell must not be empty"));
                    }
                    let args = options
                        .get::<Option<Table>>("args")?
                        .map(|table| {
                            table
                                .sequence_values::<String>()
                                .collect::<mlua::Result<Vec<_>>>()
                        })
                        .transpose()?
                        .unwrap_or_default();
                    let mut command = Command::new(&shell);
                    command.args(&args);
                    if let Some(cwd) = options.get::<Option<String>>("cwd")? {
                        command.current_dir(cwd);
                    }
                    if let Some(env) = options.get::<Option<Table>>("env")? {
                        for pair in env.pairs::<String, String>() {
                            let (name, value) = pair?;
                            command.env(name, value);
                        }
                    }
                    command
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());
                    let mut child = command.spawn().map_err(mlua::Error::external)?;
                    let pid = child.id();
                    let stdin = Arc::new(Mutex::new(child.stdin.take()));
                    let child = Arc::new(Mutex::new(child));
                    let id = {
                        let mut next = spawn_ids.borrow_mut();
                        *next = next.saturating_add(1);
                        *next
                    };
                    let on_stdout = options
                        .get::<Option<Function>>("on_stdout")?
                        .map(|callback| lua.create_registry_value(callback))
                        .transpose()?;
                    let on_stderr = options
                        .get::<Option<Function>>("on_stderr")?
                        .map(|callback| lua.create_registry_value(callback))
                        .transpose()?;
                    let on_exit = options
                        .get::<Option<Function>>("on_exit")?
                        .map(|callback| lua.create_registry_value(callback))
                        .transpose()?;
                    let stdout = child
                        .lock()
                        .map_err(|_| mlua::Error::external("spawn child poisoned"))?
                        .stdout
                        .take();
                    let stderr = child
                        .lock()
                        .map_err(|_| mlua::Error::external("spawn child poisoned"))?
                        .stderr
                        .take();
                    let stdout_open = stdout.is_some();
                    let stderr_open = stderr.is_some();
                    spawn_callbacks_sink.borrow_mut().insert(
                        id,
                        SpawnRegistration {
                            on_stdout,
                            on_stderr,
                            on_exit,
                            stdout_open,
                            stderr_open,
                            exited: false,
                        },
                    );
                    if let Some(stdout) = stdout {
                        spawn_reader_thread(id, stdout, spawn_events.clone(), false);
                    }
                    if let Some(stderr) = stderr {
                        spawn_reader_thread(id, stderr, spawn_events.clone(), true);
                    }
                    let exit_events = spawn_events.clone();
                    let exit_child = Arc::clone(&child);
                    thread::spawn(move || loop {
                        let status = exit_child
                            .lock()
                            .ok()
                            .and_then(|mut child| child.try_wait().ok().flatten());
                        if let Some(status) = status {
                            let _ = exit_events.send(SpawnEvent {
                                id,
                                kind: SpawnEventKind::Exit(status.code()),
                            });
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    });
                    let handle = Arc::new(Mutex::new(SpawnHandle { child, stdin, pid }));
                    let process = lua.create_table()?;
                    let write_stdin = Arc::clone(&handle);
                    process.set(
                        "write",
                        lua.create_function(move |_, (_self, text): (Table, String)| {
                            let handle = write_stdin
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn handle poisoned"))?;
                            let mut stdin = handle
                                .stdin
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn stdin poisoned"))?;
                            let Some(stdin) = stdin.as_mut() else {
                                return Ok(false);
                            };
                            stdin
                                .write_all(text.as_bytes())
                                .map_err(mlua::Error::external)?;
                            stdin.flush().map_err(mlua::Error::external)?;
                            Ok(true)
                        })?,
                    )?;
                    let close_stdin = Arc::clone(&handle);
                    process.set(
                        "close_stdin",
                        lua.create_function(move |_, _self: Table| {
                            let handle = close_stdin
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn handle poisoned"))?;
                            let closed = handle
                                .stdin
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn stdin poisoned"))?
                                .take()
                                .is_some();
                            Ok(closed)
                        })?,
                    )?;
                    let kill_process = Arc::clone(&handle);
                    process.set(
                        "kill",
                        lua.create_function(move |_, _self: Table| {
                            let handle = kill_process
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn handle poisoned"))?;
                            let result = handle
                                .child
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn child poisoned"))?
                                .kill()
                                .map(|_| true)
                                .map_err(mlua::Error::external);
                            result
                        })?,
                    )?;
                    let process_pid = Arc::clone(&handle);
                    process.set(
                        "pid",
                        lua.create_function(move |_, _self: Table| {
                            Ok(process_pid
                                .lock()
                                .map_err(|_| mlua::Error::external("spawn handle poisoned"))?
                                .pid)
                        })?,
                    )?;
                    Ok(process)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let terminal_module = create_terminal_module(
            &lua,
            package
                .manifest
                .permissions
                .get("terminal.control")
                .copied()
                .unwrap_or(false),
            package
                .manifest
                .permissions
                .get("terminal.read")
                .copied()
                .unwrap_or(false),
            Rc::clone(&terminal_manager),
        )?;

        let text_module = lua.create_table().map_err(lua_error)?;
        text_module
            .set(
                "width",
                lua.create_function(|_, value: String| Ok(UnicodeWidthStr::width(value.as_str())))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let editor_module = lua.create_table().map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "buffer_text",
                lua.create_function(move |_, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    Ok(editor_snapshot_source.borrow().lines.join("\n"))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "byte_to_col",
                lua.create_function(|_, (text, byte_offset): (String, usize)| {
                    Ok(text
                        .char_indices()
                        .take_while(|(offset, _)| *offset < byte_offset)
                        .count()
                        + 1)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        editor_module
            .set(
                "col_to_byte",
                lua.create_function(|_, (text, col): (String, usize)| {
                    if col <= 1 {
                        return Ok(0usize);
                    }
                    Ok(text
                        .char_indices()
                        .nth(col.saturating_sub(1))
                        .map_or(text.len(), |(offset, _)| offset))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let viewport_snapshot = Rc::clone(&editor_snapshot);
        editor_module
            .set(
                "viewport",
                lua.create_function(move |lua, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let snapshot = viewport_snapshot.borrow();
                    let viewport = lua.create_table()?;
                    viewport.set("top_line", 1usize)?;
                    viewport.set("bottom_line", snapshot.lines.len().max(1))?;
                    viewport.set("left_col", 1usize)?;
                    Ok(viewport)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "scroll_to",
                lua.create_function(move |_, line: usize| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::ScrollTo {
                            line: line.saturating_sub(1),
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "scroll_by",
                lua.create_function(move |_, delta: isize| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::ScrollBy { delta });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "set_search",
                lua.create_function(move |_, (pattern, use_regex): (String, Option<bool>)| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::SetSearch {
                            pattern,
                            use_regex: use_regex.unwrap_or(false),
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "clear_search",
                lua.create_function(move |_, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::ClearSearch);
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let editor_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "insert",
                lua.create_function(move |_, (line, col, text): (usize, usize, String)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::Insert {
                            line: line.saturating_sub(1),
                            col: col.saturating_sub(1),
                            text,
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "set_line",
                lua.create_function(move |_, (line, text): (usize, String)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::SetLine {
                            line: line.saturating_sub(1),
                            text,
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "replace",
                lua.create_function(
                    move |_,
                          (start_line, start_col, end_line, end_col, text): (
                        usize,
                        usize,
                        usize,
                        usize,
                        String,
                    )| {
                        if !editor_write {
                            return Err(mlua::Error::external("permission denied: editor.write"));
                        }
                        editor_request_sink
                            .borrow_mut()
                            .push(PluginEditorRequest::Replace {
                                start_line: start_line.saturating_sub(1),
                                start_col: start_col.saturating_sub(1),
                                end_line: end_line.saturating_sub(1),
                                end_col: end_col.saturating_sub(1),
                                text,
                            });
                        Ok(true)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "set_cursor",
                lua.create_function(move |_, (line, col): (usize, usize)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::SetCursor {
                            line: line.saturating_sub(1),
                            col: col.saturating_sub(1),
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "set_selection",
                lua.create_function(
                    move |_,
                          (start_line, start_col, end_line, end_col): (
                        usize,
                        usize,
                        usize,
                        usize,
                    )| {
                        if !editor_write {
                            return Err(mlua::Error::external("permission denied: editor.write"));
                        }
                        editor_request_sink
                            .borrow_mut()
                            .push(PluginEditorRequest::SetSelection {
                                start_line: start_line.saturating_sub(1),
                                start_col: start_col.saturating_sub(1),
                                end_line: end_line.saturating_sub(1),
                                end_col: end_col.saturating_sub(1),
                            });
                        Ok(true)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for (name, request) in [
            ("begin_undo_group", PluginEditorRequest::BeginUndoGroup),
            ("end_undo_group", PluginEditorRequest::EndUndoGroup),
            ("clear_selection", PluginEditorRequest::ClearSelection),
            ("clear_cursors", PluginEditorRequest::ClearCursors),
        ] {
            let editor_write = package
                .manifest
                .permissions
                .get("editor.write")
                .copied()
                .unwrap_or(false);
            let editor_request_sink = Rc::clone(&editor_requests);
            editor_module
                .set(
                    name,
                    lua.create_function(move |_, ()| {
                        if !editor_write {
                            return Err(mlua::Error::external("permission denied: editor.write"));
                        }
                        editor_request_sink.borrow_mut().push(request.clone());
                        Ok(true)
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let editor_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let editor_request_sink = Rc::clone(&editor_requests);
        editor_module
            .set(
                "add_cursor",
                lua.create_function(move |_, (line, col): (usize, usize)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    editor_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::AddCursor {
                            line: line.saturating_sub(1),
                            col: col.saturating_sub(1),
                        });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "buffer_lines",
                lua.create_function(move |lua, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let result = lua.create_table()?;
                    for (index, line) in editor_snapshot_source.borrow().lines.iter().enumerate() {
                        result.set(index + 1, line.as_str())?;
                    }
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "current_line",
                lua.create_function(move |_, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let snapshot = editor_snapshot_source.borrow();
                    Ok(snapshot
                        .lines
                        .get(snapshot.cursor_line)
                        .cloned()
                        .unwrap_or_default())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "line_count",
                lua.create_function(move |_, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    Ok(editor_snapshot_source.borrow().lines.len())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "get_line",
                lua.create_function(move |_, line: usize| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    Ok(editor_snapshot_source
                        .borrow()
                        .lines
                        .get(line.saturating_sub(1))
                        .cloned())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        for (name, field) in [("file_path", 0_u8), ("file_name", 1_u8), ("language", 2_u8)] {
            let editor_snapshot_source = Rc::clone(&editor_snapshot_source);
            editor_module
                .set(
                    name,
                    lua.create_function(move |_, ()| {
                        if !editor_read {
                            return Err(mlua::Error::external("permission denied: editor.read"));
                        }
                        let snapshot = editor_snapshot_source.borrow();
                        Ok(match field {
                            0 => snapshot.path.clone(),
                            1 => snapshot.file_name.clone(),
                            _ => snapshot.language.clone(),
                        })
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "cursor",
                lua.create_function(move |lua, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let snapshot = editor_snapshot_source.borrow();
                    let result = lua.create_table()?;
                    result.set("line", snapshot.cursor_line + 1)?;
                    result.set("col", snapshot.cursor_col + 1)?;
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "selection",
                lua.create_function(move |lua, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let snapshot = editor_snapshot_source.borrow();
                    let result = lua.create_table()?;
                    if let Some((start_line, start_col, end_line, end_col)) = snapshot.selection {
                        result.set("active", true)?;
                        result.set("start_line", start_line + 1)?;
                        result.set("start_col", start_col + 1)?;
                        result.set("end_line", end_line + 1)?;
                        result.set("end_col", end_col + 1)?;
                    } else {
                        result.set("active", false)?;
                    }
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "selection_text",
                lua.create_function(move |_, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    Ok(editor_snapshot_source.borrow().selection_text.clone())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_snapshot_source = Rc::clone(&editor_snapshot);
        let editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        editor_module
            .set(
                "get_cursors",
                lua.create_function(move |lua, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let snapshot = editor_snapshot_source.borrow();
                    let result = lua.create_table()?;
                    for (index, (line, col)) in snapshot.cursors.iter().enumerate() {
                        let cursor = lua.create_table()?;
                        cursor.set("line", line + 1)?;
                        cursor.set("col", col + 1)?;
                        result.set(index + 1, cursor)?;
                    }
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let diagnostics_module = lua.create_table().map_err(lua_error)?;
        let diagnostics_sink = Rc::clone(&diagnostics);
        let can_publish_diagnostics = package
            .manifest
            .permissions
            .get("editor.diagnostics")
            .copied()
            .unwrap_or(false);
        diagnostics_module
            .set(
                "publish",
                lua.create_function(move |_, (path, items): (String, Table)| {
                    if !can_publish_diagnostics {
                        return Err(mlua::Error::external(
                            "permission denied: editor.diagnostics",
                        ));
                    }
                    if path.trim().is_empty() {
                        return Err(mlua::Error::external("diagnostic path is required"));
                    }
                    let mut diagnostics = Vec::new();
                    for item in items.sequence_values::<Table>() {
                        let item = item?;
                        let line = item.get::<usize>("line")?.saturating_sub(1);
                        let col = item.get::<usize>("col")?.saturating_sub(1);
                        let end_line = item
                            .get::<Option<usize>>("end_line")?
                            .unwrap_or(line + 1)
                            .saturating_sub(1);
                        let end_col = item
                            .get::<Option<usize>>("end_col")?
                            .unwrap_or(col + 2)
                            .saturating_sub(1);
                        let severity = match item.get::<Option<String>>("severity")?.as_deref() {
                            Some("error") => 1,
                            Some("info") => 3,
                            Some("hint") => 4,
                            _ => 2,
                        };
                        diagnostics.push(PluginDiagnostic {
                            line,
                            col,
                            end_line,
                            end_col,
                            severity,
                            message: item.get::<Option<String>>("message")?.unwrap_or_default(),
                            source: item.get::<Option<String>>("source")?.unwrap_or_default(),
                        });
                    }
                    diagnostics_sink.borrow_mut().insert(path, diagnostics);
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let diagnostics_sink = Rc::clone(&diagnostics);
        let can_publish_diagnostics = package
            .manifest
            .permissions
            .get("editor.diagnostics")
            .copied()
            .unwrap_or(false);
        diagnostics_module
            .set(
                "clear",
                lua.create_function(move |_, path: Option<String>| {
                    if !can_publish_diagnostics {
                        return Err(mlua::Error::external(
                            "permission denied: editor.diagnostics",
                        ));
                    }
                    let mut diagnostics = diagnostics_sink.borrow_mut();
                    if let Some(path) = path {
                        diagnostics.remove(&path);
                    } else {
                        diagnostics.clear();
                    }
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let decorations_module = lua.create_table().map_err(lua_error)?;
        let decorations_sink = Rc::clone(&diagnostics);
        let can_decorate = package
            .manifest
            .permissions
            .get("editor.decorations")
            .copied()
            .unwrap_or(false);
        decorations_module
            .set(
                "set",
                lua.create_function(move |_, (path, ranges): (String, Table)| {
                    if !can_decorate {
                        return Err(mlua::Error::external(
                            "permission denied: editor.decorations",
                        ));
                    }
                    if path.is_empty() {
                        return Err(mlua::Error::external("decoration path is required"));
                    }
                    let mut decorations = Vec::new();
                    for item in ranges.sequence_values::<Table>() {
                        let item = item?;
                        let line = item
                            .get::<Option<usize>>("line")?
                            .ok_or_else(|| mlua::Error::external("decoration line is required"))?
                            .saturating_sub(1);
                        let end_line = item
                            .get::<Option<usize>>("end_line")?
                            .unwrap_or(line + 1)
                            .saturating_sub(1)
                            .max(line);
                        let col = item
                            .get::<Option<usize>>("col")?
                            .unwrap_or(1)
                            .saturating_sub(1);
                        let end_col = item
                            .get::<Option<usize>>("end_col")?
                            .unwrap_or(col + 1)
                            .saturating_sub(1)
                            .max(col);
                        let style = item
                            .get::<Option<String>>("style")?
                            .unwrap_or_else(|| "decoration".to_owned());
                        let note = item.get::<Option<String>>("note")?.unwrap_or_default();
                        decorations.push(PluginDiagnostic {
                            line,
                            col,
                            end_line,
                            end_col,
                            severity: 3,
                            message: note,
                            source: format!("decoration:{style}"),
                        });
                    }
                    decorations_sink.borrow_mut().insert(path, decorations);
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let decorations_sink = Rc::clone(&diagnostics);
        decorations_module
            .set(
                "clear",
                lua.create_function(move |_, path: Option<String>| {
                    if !can_decorate {
                        return Err(mlua::Error::external(
                            "permission denied: editor.decorations",
                        ));
                    }
                    if let Some(path) = path {
                        decorations_sink.borrow_mut().remove(&path);
                    } else {
                        decorations_sink.borrow_mut().clear();
                    }
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let bookmarks_module = lua.create_table().map_err(lua_error)?;
        let bookmarks_sink = Rc::clone(&bookmarks);
        let bookmark_event_sink = Rc::clone(&bookmark_events);
        let bookmark_snapshot = Rc::clone(&editor_snapshot);
        let can_bookmark = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        bookmarks_module
            .set(
                "set",
                lua.create_function(
                    move |_, (line, icon, style): (usize, String, Option<String>)| {
                        if !can_bookmark {
                            return Err(mlua::Error::external(
                                "permission denied: editor.bookmarks",
                            ));
                        }
                        let line = line.max(1);
                        let path = bookmark_snapshot.borrow().path.clone();
                        let icon = icon
                            .chars()
                            .next()
                            .map_or_else(|| "◆".to_owned(), |c| c.to_string());
                        let style = style.unwrap_or_default();
                        bookmarks_sink
                            .borrow_mut()
                            .entry(path.clone())
                            .or_default()
                            .insert(line - 1, (icon.clone(), style.clone()));
                        bookmark_event_sink.borrow_mut().push(PluginBookmarkEvent {
                            path,
                            line,
                            action: "set".to_owned(),
                            icon,
                            style,
                        });
                        Ok(true)
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let bookmarks_sink = Rc::clone(&bookmarks);
        let bookmark_event_sink = Rc::clone(&bookmark_events);
        let bookmark_snapshot = Rc::clone(&editor_snapshot);
        let can_bookmark = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        bookmarks_module
            .set(
                "remove",
                lua.create_function(move |_, line: usize| {
                    if !can_bookmark {
                        return Err(mlua::Error::external("permission denied: editor.bookmarks"));
                    }
                    let path = bookmark_snapshot.borrow().path.clone();
                    let removed = bookmarks_sink
                        .borrow_mut()
                        .get_mut(&path)
                        .and_then(|items| items.remove(&line.saturating_sub(1)));
                    if let Some((icon, style)) = removed {
                        bookmark_event_sink.borrow_mut().push(PluginBookmarkEvent {
                            path,
                            line,
                            action: "remove".to_owned(),
                            icon,
                            style,
                        });
                    }
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let bookmarks_sink = Rc::clone(&bookmarks);
        let bookmark_snapshot = Rc::clone(&editor_snapshot);
        let can_bookmark = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        bookmarks_module
            .set(
                "get",
                lua.create_function(move |lua, line: usize| {
                    if !can_bookmark {
                        return Err(mlua::Error::external("permission denied: editor.bookmarks"));
                    }
                    let path = bookmark_snapshot.borrow().path.clone();
                    let Some((icon, style)) = bookmarks_sink
                        .borrow()
                        .get(&path)
                        .and_then(|items| items.get(&line.saturating_sub(1)))
                        .cloned()
                    else {
                        return Ok(None);
                    };
                    let result = lua.create_table()?;
                    result.set("icon", icon)?;
                    result.set("style", style)?;
                    Ok(Some(result))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let bookmarks_sink = Rc::clone(&bookmarks);
        let bookmark_event_sink = Rc::clone(&bookmark_events);
        let bookmark_snapshot = Rc::clone(&editor_snapshot);
        let can_bookmark = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        bookmarks_module
            .set(
                "set_all",
                lua.create_function(move |_, items: Table| {
                    if !can_bookmark {
                        return Err(mlua::Error::external("permission denied: editor.bookmarks"));
                    }
                    let path = bookmark_snapshot.borrow().path.clone();
                    let mut bookmarks_for_file = BTreeMap::new();
                    for item in items.sequence_values::<Table>() {
                        let item = item?;
                        let line = item.get::<usize>("line")?.max(1) - 1;
                        let icon = item
                            .get::<Option<String>>("icon")?
                            .unwrap_or_else(|| "◆".to_owned());
                        let style = item.get::<Option<String>>("style")?.unwrap_or_default();
                        bookmarks_for_file.insert(line, (icon, style));
                    }
                    bookmarks_sink
                        .borrow_mut()
                        .insert(path.clone(), bookmarks_for_file);
                    bookmark_event_sink.borrow_mut().push(PluginBookmarkEvent {
                        path,
                        line: 0,
                        action: "set_all".to_owned(),
                        icon: String::new(),
                        style: String::new(),
                    });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let bookmarks_sink = Rc::clone(&bookmarks);
        let bookmark_snapshot = Rc::clone(&editor_snapshot);
        let can_bookmark = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        bookmarks_module
            .set(
                "get_all",
                lua.create_function(move |lua, ()| {
                    if !can_bookmark {
                        return Err(mlua::Error::external("permission denied: editor.bookmarks"));
                    }
                    let path = bookmark_snapshot.borrow().path.clone();
                    let result = lua.create_table()?;
                    if let Some(items) = bookmarks_sink.borrow().get(&path) {
                        for (index, (line, (icon, style))) in items.iter().enumerate() {
                            let item = lua.create_table()?;
                            item.set("line", line + 1)?;
                            item.set("icon", icon.as_str())?;
                            item.set("style", style.as_str())?;
                            result.set(index + 1, item)?;
                        }
                    }
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let bookmarks_sink = Rc::clone(&bookmarks);
        let bookmark_event_sink = Rc::clone(&bookmark_events);
        let bookmark_snapshot = Rc::clone(&editor_snapshot);
        let can_bookmark = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        bookmarks_module
            .set(
                "clear",
                lua.create_function(move |_, ()| {
                    if !can_bookmark {
                        return Err(mlua::Error::external("permission denied: editor.bookmarks"));
                    }
                    let path = bookmark_snapshot.borrow().path.clone();
                    bookmarks_sink.borrow_mut().remove(&path);
                    bookmark_event_sink.borrow_mut().push(PluginBookmarkEvent {
                        path,
                        line: 0,
                        action: "clear".to_owned(),
                        icon: String::new(),
                        style: String::new(),
                    });
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        text_module
            .set(
                "truncate",
                lua.create_function(
                    |_, (value, width, ellipsis): (String, usize, Option<String>)| {
                        let ellipsis = ellipsis.unwrap_or_else(|| "…".to_owned());
                        if UnicodeWidthStr::width(value.as_str()) <= width {
                            return Ok(value);
                        }
                        let ellipsis_width = UnicodeWidthStr::width(ellipsis.as_str());
                        let keep_width = width.saturating_sub(ellipsis_width);
                        let mut used = 0;
                        let prefix = value
                            .chars()
                            .take_while(|character| {
                                let character_width =
                                    UnicodeWidthChar::width(*character).unwrap_or(0);
                                if used + character_width > keep_width {
                                    return false;
                                }
                                used += character_width;
                                true
                            })
                            .collect::<String>();
                        Ok(format!("{prefix}{ellipsis}"))
                    },
                )
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let fs_module = lua.create_table().map_err(lua_error)?;
        let fs_root = package.root.clone();
        let can_read = package
            .manifest
            .permissions
            .get("fs.read")
            .copied()
            .unwrap_or(false);
        let read_root = fs_root.clone();
        fs_module
            .set(
                "read",
                lua.create_function(move |_, relative: String| {
                    if !can_read {
                        return Err(mlua::Error::external("permission denied: fs.read"));
                    }
                    ensure_inside(&read_root, &relative)
                        .map_err(|error| mlua::Error::external(error.to_string()))?;
                    fs::read_to_string(read_root.join(relative)).map_err(mlua::Error::external)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let can_write = package
            .manifest
            .permissions
            .get("fs.write")
            .copied()
            .unwrap_or(false);
        fs_module
            .set(
                "write",
                lua.create_function(move |_, (relative, contents): (String, String)| {
                    if !can_write {
                        return Err(mlua::Error::external("permission denied: fs.write"));
                    }
                    ensure_inside(&fs_root, &relative)
                        .map_err(|error| mlua::Error::external(error.to_string()))?;
                    fs::write(fs_root.join(relative), contents).map_err(mlua::Error::external)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let net_module = lua.create_table().map_err(lua_error)?;
        let network_allowed = package
            .manifest
            .permissions
            .get("network.http")
            .copied()
            .unwrap_or(false);
        let network_hosts = package.manifest.network_http_hosts.clone();
        let net_get = {
            let can_use = network_allowed;
            let allowed_hosts = network_hosts.clone();
            lua.create_function(move |lua, (url, options): (String, Option<Table>)| {
                if !can_use {
                    return Err(mlua::Error::external("permission denied: network.http"));
                }
                if !network_target_allowed(&url, &allowed_hosts) {
                    return net_response_table(
                        lua,
                        NetResponse {
                            status: 0,
                            body: String::new(),
                            headers: BTreeMap::new(),
                            error: Some("network destination is not allowed".to_owned()),
                        },
                    );
                }
                let (headers, body) = http_options(options).map_err(mlua::Error::external)?;
                let response = http_request("GET", &url, &headers, body.as_deref(), &allowed_hosts);
                net_response_table(lua, response)
            })
            .map_err(lua_error)?
        };
        net_module.set("get", net_get).map_err(lua_error)?;
        let net_post = {
            let can_use = network_allowed;
            let allowed_hosts = network_hosts.clone();
            lua.create_function(move |lua, (url, options): (String, Option<Table>)| {
                if !can_use {
                    return Err(mlua::Error::external("permission denied: network.http"));
                }
                if !network_target_allowed(&url, &allowed_hosts) {
                    return net_response_table(
                        lua,
                        NetResponse {
                            status: 0,
                            body: String::new(),
                            headers: BTreeMap::new(),
                            error: Some("network destination is not allowed".to_owned()),
                        },
                    );
                }
                let (headers, body) = http_options(options).map_err(mlua::Error::external)?;
                let response =
                    http_request("POST", &url, &headers, body.as_deref(), &allowed_hosts);
                net_response_table(lua, response)
            })
            .map_err(lua_error)?
        };
        net_module.set("post", net_post).map_err(lua_error)?;
        let net_async_callbacks = Rc::clone(&net_callbacks);
        let net_async_ids = Rc::clone(&next_net_id);
        let net_async_tx = net_tx.clone();
        let net_get_async_allowed = network_allowed;
        let net_get_async_hosts = network_hosts.clone();
        net_module
            .set(
                "get_async",
                lua.create_function(move |lua, (url, args): (String, MultiValue)| {
                    if !net_get_async_allowed {
                        return Err(mlua::Error::external("permission denied: network.http"));
                    }
                    let (options, callback) = net_async_args(lua, args)?;
                    let (headers, body) = http_options(options).map_err(mlua::Error::external)?;
                    let id = {
                        let mut next = net_async_ids.borrow_mut();
                        *next = next.saturating_add(1);
                        *next
                    };
                    net_async_callbacks
                        .borrow_mut()
                        .insert(id, lua.create_registry_value(callback)?);
                    let sender = net_async_tx.clone();
                    let allowed_hosts = net_get_async_hosts.clone();
                    let allowed = network_target_allowed(&url, &allowed_hosts);
                    thread::spawn(move || {
                        let response = if allowed {
                            http_request("GET", &url, &headers, body.as_deref(), &allowed_hosts)
                        } else {
                            denied_network_response()
                        };
                        let _ = sender.send(NetCompletion { id, response });
                    });
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let net_async_callbacks = Rc::clone(&net_callbacks);
        let net_async_ids = Rc::clone(&next_net_id);
        let net_async_tx = net_tx;
        let net_post_async_allowed = network_allowed;
        let net_post_async_hosts = network_hosts;
        net_module
            .set(
                "post_async",
                lua.create_function(move |lua, (url, args): (String, MultiValue)| {
                    if !net_post_async_allowed {
                        return Err(mlua::Error::external("permission denied: network.http"));
                    }
                    let (options, callback) = net_async_args(lua, args)?;
                    let (headers, body) = http_options(options).map_err(mlua::Error::external)?;
                    let id = {
                        let mut next = net_async_ids.borrow_mut();
                        *next = next.saturating_add(1);
                        *next
                    };
                    net_async_callbacks
                        .borrow_mut()
                        .insert(id, lua.create_registry_value(callback)?);
                    let sender = net_async_tx.clone();
                    let allowed_hosts = net_post_async_hosts.clone();
                    let allowed = network_target_allowed(&url, &allowed_hosts);
                    thread::spawn(move || {
                        let response = if allowed {
                            http_request("POST", &url, &headers, body.as_deref(), &allowed_hosts)
                        } else {
                            denied_network_response()
                        };
                        let _ = sender.send(NetCompletion { id, response });
                    });
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let globals = lua.globals();
        let package_table = globals.get::<Table>("package").map_err(lua_error)?;
        let preload = package_table.get::<Table>("preload").map_err(lua_error)?;
        let module_for_require = module.clone();
        preload
            .set(
                "ttt",
                lua.create_function(move |_lua, ()| Ok(module_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let json_for_require = json_module.clone();
        preload
            .set(
                "ttt.json",
                lua.create_function(move |_, ()| Ok(json_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let system_for_require = system_module.clone();
        preload
            .set(
                "ttt.system",
                lua.create_function(move |_, ()| Ok(system_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let terminal_for_require = terminal_module.clone();
        preload
            .set(
                "ttt.terminal",
                lua.create_function(move |_, ()| Ok(terminal_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let fs_for_require = fs_module.clone();
        preload
            .set(
                "ttt.fs",
                lua.create_function(move |_, ()| Ok(fs_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let text_for_require = text_module.clone();
        preload
            .set(
                "ttt.text",
                lua.create_function(move |_, ()| Ok(text_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let editor_for_require = editor_module.clone();
        preload
            .set(
                "ttt.editor",
                lua.create_function(move |_, ()| Ok(editor_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let diagnostics_for_require = diagnostics_module.clone();
        preload
            .set(
                "ttt.diagnostics",
                lua.create_function(move |_, ()| Ok(diagnostics_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let decorations_for_require = decorations_module.clone();
        preload
            .set(
                "ttt.decorations",
                lua.create_function(move |_, ()| Ok(decorations_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let bookmarks_for_require = bookmarks_module.clone();
        preload
            .set(
                "ttt.bookmarks",
                lua.create_function(move |_, ()| Ok(bookmarks_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let events_for_require = events_module.clone();
        preload
            .set(
                "ttt.events",
                lua.create_function(move |_, ()| Ok(events_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let automation_for_require = automation_module.clone();
        preload
            .set(
                "ttt.automation",
                lua.create_function(move |_, ()| Ok(automation_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let rpc_for_require = rpc_module.clone();
        preload
            .set(
                "ttt.rpc",
                lua.create_function(move |_, ()| Ok(rpc_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let command_line_for_require = command_line_module.clone();
        preload
            .set(
                "ttt.command_line",
                lua.create_function(move |_, ()| Ok(command_line_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let net_for_require = net_module.clone();
        preload
            .set(
                "ttt.net",
                lua.create_function(move |_, ()| Ok(net_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        lua.load(&source)
            .set_name(entry.to_string_lossy())
            .exec()
            .map_err(lua_error)?;
        Ok(Self {
            _lua: lua,
            registration,
            logs,
            command_callbacks,
            event_callbacks,
            panel_callbacks,
            file_handler_callbacks,
            file_close_callbacks,
            next_file_close_id,
            last_file_close_id: RefCell::new(None),
            last_file_title: RefCell::new(None),
            async_rx,
            async_callbacks,
            net_rx,
            net_callbacks,
            spawn_rx,
            spawn_callbacks,
            panel_requests,
            clipboard_text,
            editor_snapshot,
            editor_requests,
            diagnostics,
            bookmarks,
            bookmark_events,
            timers,
            automation_requests,
            automation_callbacks,
            dialog_callbacks,
            command_line_callbacks,
            command_line_active,
            custom_dialog_callbacks,
            dialog_widgets,
            rpc_rx,
            rpc_connections,
            rpc_pending,
            open_tabs,
        })
    }
    pub fn registration(&self) -> PluginRegistration {
        self.registration.borrow().clone()
    }
    pub fn logs(&self) -> Vec<String> {
        self.logs.borrow().clone()
    }
    pub fn drain_panel_requests(&self) -> Vec<PluginPanelRequest> {
        self.panel_requests.borrow_mut().drain(..).collect()
    }
    pub fn enqueue_panel_request(&self, request: PluginPanelRequest) {
        self.panel_requests.borrow_mut().push(request);
    }
    pub fn resolve_dialog(&self, id: u64, confirmed: bool) -> AppResult<bool> {
        let Some(key) = self.dialog_callbacks.borrow_mut().remove(&id) else {
            return Ok(false);
        };
        if confirmed {
            let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
            callback
                .call::<()>(())
                .map_err(|error| plugin_error(format!("confirm callback failed: {error}")))?;
        }
        Ok(true)
    }
    pub fn close_custom_dialog(&self, id: u64) -> AppResult<bool> {
        self.panel_callbacks
            .borrow_mut()
            .remove(&format!("__dialog:{id}"));
        let Some(callbacks) = self.custom_dialog_callbacks.borrow_mut().remove(&id) else {
            return Ok(false);
        };
        if let Some(key) = callbacks.on_close {
            let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
            callback
                .call::<()>(())
                .map_err(|error| plugin_error(format!("dialog on_close failed: {error}")))?;
        }
        Ok(true)
    }
    pub fn dialog_input(&self, id: u64, text: String) -> AppResult<bool> {
        let callback = {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let state = widgets.entry(id).or_default();
            state.input_text = text.clone();
            state
                .input_on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
        };
        let Some(callback) = callback else {
            return Ok(false);
        };
        callback
            .call::<()>(text)
            .map_err(|error| plugin_error(format!("dialog input callback failed: {error}")))?;
        Ok(true)
    }
    pub fn dialog_click(&self, id: u64) -> AppResult<bool> {
        let callback = {
            let widgets = self.dialog_widgets.borrow();
            widgets
                .get(&id)
                .and_then(|state| state.button_on_click.as_ref())
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
        };
        let Some(callback) = callback else {
            return Ok(false);
        };
        callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("dialog button callback failed: {error}")))?;
        Ok(true)
    }
    pub fn dialog_focus_next(&self, id: u64) -> bool {
        let mut widgets = self.dialog_widgets.borrow_mut();
        let Some(state) = widgets.get_mut(&id) else {
            return false;
        };
        if state.button_on_click.is_some() {
            state.focus = match state.focus {
                PluginDialogFocus::Input => PluginDialogFocus::Button,
                PluginDialogFocus::Button => PluginDialogFocus::Input,
            };
        }
        true
    }
    pub fn dialog_focus_button(&self, id: u64) -> bool {
        let mut widgets = self.dialog_widgets.borrow_mut();
        let Some(state) = widgets.get_mut(&id) else {
            return false;
        };
        if state.button_on_click.is_none() {
            return false;
        }
        state.focus = PluginDialogFocus::Button;
        true
    }
    pub fn dialog_activate(&self, id: u64) -> AppResult<bool> {
        let focus = self
            .dialog_widgets
            .borrow()
            .get(&id)
            .map(|state| state.focus);
        if focus == Some(PluginDialogFocus::Button) {
            self.dialog_click(id)
        } else {
            Ok(false)
        }
    }
    pub fn dialog_append(&self, id: u64, value: char) -> AppResult<bool> {
        let text = {
            let widgets = self.dialog_widgets.borrow();
            let Some(state) = widgets.get(&id) else {
                return Ok(false);
            };
            if state.focus != PluginDialogFocus::Input {
                return Ok(false);
            }
            format!("{}{}", state.input_text, value)
        };
        self.dialog_input(id, text)
    }
    pub fn dialog_backspace(&self, id: u64) -> AppResult<bool> {
        let text = {
            let widgets = self.dialog_widgets.borrow();
            let Some(state) = widgets.get(&id) else {
                return Ok(false);
            };
            if state.focus != PluginDialogFocus::Input {
                return Ok(false);
            }
            let mut text = state.input_text.clone();
            text.pop();
            text
        };
        self.dialog_input(id, text)
    }
    pub fn command_line_change(&self, text: &str) -> AppResult<()> {
        let callback: Option<Function> = {
            let callbacks = self.command_line_callbacks.borrow();
            let key = callbacks
                .values()
                .next()
                .and_then(|callbacks| callbacks.on_change.as_ref());
            key.map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
        };
        let Some(callback) = callback else {
            return Ok(());
        };
        callback
            .call::<()>(text.to_owned())
            .map_err(|error| plugin_error(format!("command line on_change failed: {error}")))
    }
    pub fn command_line_finish(&self, text: &str, confirmed: bool) -> AppResult<()> {
        let callbacks = self
            .command_line_callbacks
            .borrow_mut()
            .pop_first()
            .map(|(_, value)| value);
        *self.command_line_active.borrow_mut() = false;
        let Some(callbacks) = callbacks else {
            return Ok(());
        };
        let key = if confirmed {
            callbacks.on_submit
        } else {
            callbacks.on_cancel
        };
        let Some(key) = key else {
            return Ok(());
        };
        let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
        if confirmed {
            callback
                .call::<()>(text.to_owned())
                .map_err(|error| plugin_error(format!("command line on_submit failed: {error}")))
        } else {
            callback
                .call::<()>(())
                .map_err(|error| plugin_error(format!("command line on_cancel failed: {error}")))
        }
    }
    pub fn command_line_key(
        &self,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let callback: Option<Function> = {
            let callbacks = self.command_line_callbacks.borrow();
            let key = callbacks
                .values()
                .next()
                .and_then(|callbacks| callbacks.on_key.as_ref());
            key.map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
        };
        let Some(callback) = callback else {
            return Ok(false);
        };
        let event = self._lua.create_table().map_err(lua_error)?;
        event.set("type", "key").map_err(lua_error)?;
        event.set("key", key_name).map_err(lua_error)?;
        if let Some(rune) = rune {
            event.set("rune", rune.to_string()).map_err(lua_error)?;
        }
        if !modifier.is_empty() {
            event.set("mod", modifier).map_err(lua_error)?;
        }
        callback
            .call::<bool>(event)
            .map_err(|error| plugin_error(format!("command line on_key failed: {error}")))
    }
    pub fn set_clipboard_text(&self, text: &str) {
        *self.clipboard_text.borrow_mut() = text.to_owned();
    }
    pub fn set_editor_snapshot(&self, snapshot: PluginEditorSnapshot) {
        *self.editor_snapshot.borrow_mut() = snapshot;
    }
    pub fn drain_editor_requests(&self) -> Vec<PluginEditorRequest> {
        self.editor_requests.borrow_mut().drain(..).collect()
    }
    pub fn diagnostic_severity(&self, path: &str, line: usize) -> Option<u8> {
        self.diagnostics.borrow().get(path).and_then(|items| {
            items
                .iter()
                .filter(|item| (item.line..=item.end_line).contains(&line))
                .map(|item| item.severity)
                .min()
        })
    }
    pub fn diagnostic_message(&self, path: &str, line: usize) -> Option<String> {
        self.diagnostics.borrow().get(path).and_then(|items| {
            items
                .iter()
                .find(|item| (item.line..=item.end_line).contains(&line))
                .map(|item| item.message.clone())
        })
    }
    pub fn bookmark_marker(&self, path: &str, line: usize) -> Option<String> {
        self.bookmarks
            .borrow()
            .get(path)
            .and_then(|items| items.get(&line))
            .map(|(icon, _)| icon.clone())
    }
    pub fn drain_bookmark_events(&self) -> Vec<PluginBookmarkEvent> {
        self.bookmark_events.borrow_mut().drain(..).collect()
    }
    pub fn drain_automation_requests(&self) -> Vec<PluginAutomationTask> {
        self.automation_requests
            .borrow_mut()
            .drain(..)
            .map(|request| {
                self.automation_callbacks
                    .borrow_mut()
                    .insert(request.task.id, request.callback);
                request.task
            })
            .collect()
    }
    pub fn complete_automation(
        &self,
        id: u64,
        result: JsonValue,
        error: Option<String>,
    ) -> AppResult<bool> {
        let Some(callback) = self.automation_callbacks.borrow_mut().remove(&id) else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&callback).map_err(lua_error)?;
        let mut values = MultiValue::new();
        values.push_back(self._lua.to_value(&result).map_err(lua_error)?);
        values.push_back(if let Some(error) = error {
            LuaValue::String(self._lua.create_string(error).map_err(lua_error)?)
        } else {
            LuaValue::Nil
        });
        callback.call::<()>(values).map_err(lua_error)?;
        Ok(true)
    }
    pub fn execute_command(&self, id: &str) -> AppResult<bool> {
        let callbacks = self.command_callbacks.borrow();
        let Some(key) = callbacks.get(id) else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(key).map_err(lua_error)?;
        callback.call::<()>(()).map_err(lua_error)?;
        Ok(true)
    }
    pub fn trigger_keybinding(&self, key: &str) -> AppResult<bool> {
        let command = self
            .registration
            .borrow()
            .keybindings
            .iter()
            .find(|binding| binding.key == key)
            .map(|binding| binding.command.clone());
        command.map_or(Ok(false), |command| self.execute_command(&command))
    }
    #[allow(dead_code)]
    pub fn emit(&self, event: &str) -> AppResult<bool> {
        self.emit_with_payload(event, &[])
    }
    pub fn emit_with_payload(&self, event: &str, payload: &[PluginEventArg]) -> AppResult<bool> {
        let callbacks = self.event_callbacks.borrow();
        let Some(keys) = callbacks.get(event) else {
            return Ok(false);
        };
        for key in keys {
            let callback: Function = self._lua.registry_value(key).map_err(lua_error)?;
            let mut arguments = MultiValue::new();
            for value in payload {
                arguments.push_back(match value {
                    PluginEventArg::Text(value) => {
                        LuaValue::String(self._lua.create_string(value).map_err(lua_error)?)
                    }
                    PluginEventArg::Number(value) => LuaValue::Integer(*value as i64),
                    PluginEventArg::Bool(value) => LuaValue::Boolean(*value),
                });
            }
            callback.call::<()>(arguments).map_err(lua_error)?;
        }
        Ok(true)
    }
    pub fn poll_async(&self) -> AppResult<usize> {
        let mut completed = self.poll_timers()?;
        completed += self.poll_rpc_events()?;
        while let Ok(completion) = self.async_rx.try_recv() {
            let Some(key) = self.async_callbacks.borrow_mut().remove(&completion.id) else {
                continue;
            };
            let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
            let result =
                completion
                    .result
                    .map_err(plugin_error)
                    .and_then(|(output, timed_out)| {
                        command_result(&self._lua, output, timed_out).map_err(lua_error)
                    })?;
            callback
                .call::<LuaValue>(result)
                .map_err(|error| plugin_error(format!("exec_async callback failed: {error}")))?;
            completed += 1;
        }
        while let Ok(completion) = self.net_rx.try_recv() {
            let Some(key) = self.net_callbacks.borrow_mut().remove(&completion.id) else {
                continue;
            };
            let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
            let response =
                net_response_table(&self._lua, completion.response).map_err(lua_error)?;
            callback
                .call::<()>(response)
                .map_err(|error| plugin_error(format!("network callback failed: {error}")))?;
            completed += 1;
        }
        while let Ok(event) = self.spawn_rx.try_recv() {
            let callback: Option<Function> = {
                let callbacks = self.spawn_callbacks.borrow();
                let key = match &event.kind {
                    SpawnEventKind::Stdout(_) => callbacks
                        .get(&event.id)
                        .and_then(|registration| registration.on_stdout.as_ref()),
                    SpawnEventKind::Stderr(_) => callbacks
                        .get(&event.id)
                        .and_then(|registration| registration.on_stderr.as_ref()),
                    SpawnEventKind::Exit(_) => callbacks
                        .get(&event.id)
                        .and_then(|registration| registration.on_exit.as_ref()),
                    SpawnEventKind::StreamClosed { .. } => None,
                };
                key.map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?
            };
            let is_stream_closed = matches!(&event.kind, SpawnEventKind::StreamClosed { .. });
            let is_exit = matches!(&event.kind, SpawnEventKind::Exit(_));
            let stream_closed_stderr = match &event.kind {
                SpawnEventKind::StreamClosed { stderr } => Some(*stderr),
                _ => None,
            };
            if let Some(callback) = callback {
                match event.kind {
                    SpawnEventKind::Stdout(text) | SpawnEventKind::Stderr(text) => {
                        callback.call::<()>(text).map_err(|error| {
                            plugin_error(format!("spawn output callback failed: {error}"))
                        })?;
                    }
                    SpawnEventKind::Exit(code) => {
                        callback
                            .call::<()>((code.unwrap_or(-1), LuaValue::Nil))
                            .map_err(|error| {
                                plugin_error(format!("spawn exit callback failed: {error}"))
                            })?;
                    }
                    SpawnEventKind::StreamClosed { .. } => {}
                }
            }
            if is_stream_closed {
                let mut callbacks = self.spawn_callbacks.borrow_mut();
                let Some(registration) = callbacks.get_mut(&event.id) else {
                    continue;
                };
                if let Some(stderr) = stream_closed_stderr {
                    if stderr {
                        registration.stderr_open = false;
                    } else {
                        registration.stdout_open = false;
                    }
                }
                if registration.exited && !registration.stdout_open && !registration.stderr_open {
                    callbacks.remove(&event.id);
                }
            } else if is_exit {
                let mut callbacks = self.spawn_callbacks.borrow_mut();
                let Some(registration) = callbacks.get_mut(&event.id) else {
                    continue;
                };
                registration.exited = true;
                if !registration.stdout_open && !registration.stderr_open {
                    callbacks.remove(&event.id);
                }
            }
            completed += 1;
        }
        Ok(completed)
    }
    fn poll_rpc_events(&self) -> AppResult<usize> {
        let mut completed = 0;
        while let Ok(event) = self.rpc_rx.try_recv() {
            match event.kind {
                RpcEventKind::Response { id, message } => {
                    let Some(key) = self
                        .rpc_pending
                        .borrow_mut()
                        .remove(&(event.connection_id, id))
                    else {
                        continue;
                    };
                    let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
                    let error = message
                        .get("error")
                        .and_then(|value| value.get("message"))
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned);
                    let result = message.get("result").cloned().unwrap_or(JsonValue::Null);
                    let result: LuaValue = self._lua.to_value(&result).map_err(lua_error)?;
                    let error_value = if let Some(error) = error {
                        LuaValue::String(self._lua.create_string(error).map_err(lua_error)?)
                    } else {
                        LuaValue::Nil
                    };
                    callback
                        .call::<()>(if error_value == LuaValue::Nil {
                            (result, error_value)
                        } else {
                            (LuaValue::Nil, error_value)
                        })
                        .map_err(|error| {
                            plugin_error(format!("RPC response callback failed: {error}"))
                        })?;
                    completed += 1;
                }
                RpcEventKind::Notification { method, params } => {
                    let callback: Option<Function> = {
                        let connections = self.rpc_connections.borrow();
                        let key = connections
                            .get(&event.connection_id)
                            .and_then(|registration| registration.on_notification.as_ref());
                        key.map(|key| self._lua.registry_value(key))
                            .transpose()
                            .map_err(lua_error)?
                    };
                    let Some(callback) = callback else {
                        continue;
                    };
                    callback
                        .call::<()>((method, self._lua.to_value(&params).map_err(lua_error)?))
                        .map_err(|error| {
                            plugin_error(format!("RPC notification callback failed: {error}"))
                        })?;
                    completed += 1;
                }
                RpcEventKind::Request { id, method, params } => {
                    let callback: Option<Function> = {
                        let connections = self.rpc_connections.borrow();
                        let key = connections
                            .get(&event.connection_id)
                            .and_then(|registration| registration.on_request.as_ref());
                        key.map(|key| self._lua.registry_value(key))
                            .transpose()
                            .map_err(lua_error)?
                    };
                    let writer = self
                        .rpc_connections
                        .borrow()
                        .get(&event.connection_id)
                        .map(|registration| Arc::clone(&registration.writer));
                    let Some(writer) = writer else {
                        continue;
                    };
                    let response = if let Some(callback) = callback {
                        match callback.call::<LuaValue>((
                            method,
                            self._lua.to_value(&params).map_err(lua_error)?,
                        )) {
                            Ok(value) => {
                                json!({"jsonrpc":"2.0","id":id,"result":self._lua.from_value::<JsonValue>(value).map_err(lua_error)?})
                            }
                            Err(error) => {
                                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":error.to_string()}})
                            }
                        }
                    } else {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"RPC request handler unavailable"}})
                    };
                    let mut writer = writer
                        .lock()
                        .map_err(|_| plugin_error("RPC writer poisoned"))?;
                    write_rpc_message(&mut writer, &response).map_err(plugin_error)?;
                    completed += 1;
                }
                RpcEventKind::Closed => {
                    let registration = self
                        .rpc_connections
                        .borrow_mut()
                        .remove(&event.connection_id);
                    let pending = {
                        let mut pending = self.rpc_pending.borrow_mut();
                        let ids = pending
                            .keys()
                            .filter(|(connection_id, _)| *connection_id == event.connection_id)
                            .copied()
                            .collect::<Vec<_>>();
                        ids.into_iter()
                            .filter_map(|id| pending.remove(&id))
                            .collect::<Vec<_>>()
                    };
                    if let Some(registration) = registration {
                        if let Some(key) = registration.on_close {
                            let callback: Function =
                                self._lua.registry_value(&key).map_err(lua_error)?;
                            callback.call::<()>(()).map_err(lua_error)?;
                        }
                    }
                    for key in pending {
                        let callback: Function =
                            self._lua.registry_value(&key).map_err(lua_error)?;
                        callback
                            .call::<()>((
                                LuaValue::Nil,
                                self._lua
                                    .create_string("RPC connection closed")
                                    .map_err(lua_error)?,
                            ))
                            .map_err(lua_error)?;
                    }
                    completed += 1;
                }
            }
        }
        Ok(completed)
    }
    fn poll_timers(&self) -> AppResult<usize> {
        let now = Instant::now();
        let due = self
            .timers
            .borrow()
            .iter()
            .filter_map(|(id, timer)| (timer.next <= now).then_some(*id))
            .collect::<Vec<_>>();
        let mut completed = 0;
        for id in due {
            let callback: Function = {
                let mut timers = self.timers.borrow_mut();
                let Some(timer) = timers.get_mut(&id) else {
                    continue;
                };
                if let Some(interval) = timer.interval {
                    timer.next = now + interval;
                    self._lua
                        .registry_value(&timer.callback)
                        .map_err(lua_error)?
                } else {
                    let _ = timer;
                    let timer = timers
                        .remove(&id)
                        .ok_or_else(|| plugin_error("timer disappeared"))?;
                    self._lua
                        .registry_value(&timer.callback)
                        .map_err(lua_error)?
                }
            };
            callback
                .call::<()>(())
                .map_err(|error| plugin_error(format!("timer callback failed: {error}")))?;
            completed += 1;
        }
        Ok(completed)
    }
    pub fn render_panel(&self, id: &str) -> AppResult<Option<String>> {
        let dialog_id = id
            .strip_prefix("__dialog:")
            .and_then(|value| value.parse::<u64>().ok());
        if let Some(dialog_id) = dialog_id {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let state = widgets.entry(dialog_id).or_default();
            state.input_on_change = None;
            state.button_label = None;
            state.button_on_click = None;
        }
        let callbacks = self.panel_callbacks.borrow();
        let Some(key) = callbacks.get(id) else {
            return Ok(None);
        };
        let callback: Function = self._lua.registry_value(key).map_err(lua_error)?;
        let lines = Rc::new(RefCell::new(Vec::<String>::new()));
        let panel = self._lua.create_table().map_err(lua_error)?;
        for name in [
            "label", "text", "markdown", "heading", "line", "status", "tree",
        ] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                            if let Some(value) = value {
                                sink.borrow_mut().push(lua_value_text(lua, value));
                            }
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let input_sink = Rc::clone(&lines);
        let input_widgets = Rc::clone(&self.dialog_widgets);
        panel
            .set(
                "input",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        let Some(LuaValue::Table(value)) = value else {
                            return Ok(());
                        };
                        let text = value
                            .get::<Option<String>>("text")?
                            .or_else(|| value.get::<Option<String>>("value").ok().flatten())
                            .unwrap_or_default();
                        if let Some(dialog_id) = dialog_id {
                            let mut widgets = input_widgets.borrow_mut();
                            let state = widgets.entry(dialog_id).or_default();
                            if !state.input_initialized {
                                state.input_text = text.clone();
                                state.input_initialized = true;
                            }
                            if let Some(callback) = value.get::<Option<Function>>("on_change")? {
                                state.input_on_change = Some(lua.create_registry_value(callback)?);
                            }
                        }
                        let placeholder = value
                            .get::<Option<String>>("placeholder")?
                            .unwrap_or_default();
                        let displayed = if let Some(dialog_id) = dialog_id {
                            input_widgets
                                .borrow()
                                .get(&dialog_id)
                                .map(|state| state.input_text.clone())
                                .unwrap_or(text)
                        } else {
                            text
                        };
                        input_sink.borrow_mut().push(
                            if displayed.is_empty() && !placeholder.is_empty() {
                                format!("[{}]", placeholder)
                            } else {
                                format!("❯ {displayed}")
                            },
                        );
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let button_sink = Rc::clone(&lines);
        let button_widgets = Rc::clone(&self.dialog_widgets);
        panel
            .set(
                "button",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        let Some(LuaValue::Table(value)) = value else {
                            return Ok(());
                        };
                        let label = value
                            .get::<Option<String>>("label")?
                            .unwrap_or_else(|| "Button".to_owned());
                        if let Some(dialog_id) = dialog_id {
                            let mut widgets = button_widgets.borrow_mut();
                            let state = widgets.entry(dialog_id).or_default();
                            state.button_label = Some(label.clone());
                            if let Some(callback) = value.get::<Option<Function>>("on_click")? {
                                state.button_on_click = Some(lua.create_registry_value(callback)?);
                            }
                        }
                        button_sink.borrow_mut().push(format!("[ {label} ]"));
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for (name, table_mode) in [("list", false), ("table", true)] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                            if let Some(value) = value {
                                sink.borrow_mut()
                                    .push(collection_widget_text(lua, value, table_mode));
                            }
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        for name in ["tabs", "scrollview"] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                            if let Some(value) = value {
                                sink.borrow_mut()
                                    .push(collection_widget_text(lua, value, false));
                            }
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let pager_sink = Rc::clone(&lines);
        panel
            .set(
                "pager",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            let page = value.get::<Option<u64>>("page").ok().flatten();
                            let total = value
                                .get::<Option<u64>>("pages")
                                .ok()
                                .flatten()
                                .or_else(|| value.get::<Option<u64>>("total").ok().flatten());
                            if let (Some(page), Some(total)) = (page, total) {
                                pager_sink.borrow_mut().push(format!("Page {page}/{total}"));
                            } else {
                                pager_sink
                                    .borrow_mut()
                                    .push(lua_value_text(lua, LuaValue::Table(value)));
                            }
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for name in [
            "badge",
            "border",
            "dialog",
            "dropdown",
            "hstack",
            "password",
            "raw",
            "row_action",
            "title",
            "vstack",
        ] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(
                            move |lua, (_panel, values): (Table, Variadic<LuaValue>)| {
                                if !values.is_empty() {
                                    sink.borrow_mut().push(
                                        values
                                            .into_iter()
                                            .map(|value| lua_value_text(lua, value))
                                            .collect::<Vec<_>>()
                                            .join(" "),
                                    );
                                }
                                Ok(())
                            },
                        )
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let separator_sink = Rc::clone(&lines);
        panel
            .set(
                "separator",
                self._lua
                    .create_function(move |_, _panel: Table| {
                        separator_sink.borrow_mut().push("────────".to_owned());
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        panel
            .set(
                "redraw",
                self._lua
                    .create_function(|_, _panel: Table| Ok(()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let value = callback.call::<LuaValue>(panel).map_err(lua_error)?;
        if let LuaValue::String(text) = value {
            lines.borrow_mut().push(
                text.to_str()
                    .map_or_else(|_| String::new(), |s| s.to_owned()),
            );
        }
        let rendered = lines.borrow().join("\n");
        Ok(Some(rendered))
    }

    pub fn open_file(&self, path: &Path) -> AppResult<Option<String>> {
        *self.last_file_close_id.borrow_mut() = None;
        *self.last_file_title.borrow_mut() = None;
        let Some(extension) = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
        else {
            return Ok(None);
        };
        let handler = self
            .registration
            .borrow()
            .file_handlers
            .iter()
            .find(|handler| {
                handler.extensions.iter().any(|candidate| {
                    candidate
                        .trim_start_matches('.')
                        .eq_ignore_ascii_case(&extension)
                })
            })
            .cloned();
        let Some(handler) = handler else {
            return Ok(None);
        };
        let callback: Function = {
            let callbacks = self.file_handler_callbacks.borrow();
            let Some(key) = callbacks.get(&handler.id) else {
                return Ok(None);
            };
            self._lua.registry_value(key).map_err(lua_error)?
        };
        let tab = self._lua.create_table().map_err(lua_error)?;
        tab.set("id", handler.id.clone()).map_err(lua_error)?;
        let tab_title = Rc::new(RefCell::new(handler.title.clone()));
        tab.set("title", tab_title.borrow().clone())
            .map_err(lua_error)?;
        tab.set(
            "focus",
            self._lua
                .create_function(|_, _tab: Table| Ok(()))
                .map_err(lua_error)?,
        )
        .map_err(lua_error)?;
        let title_sink = Rc::clone(&tab_title);
        tab.set(
            "set_title",
            self._lua
                .create_function(move |_, (_tab, title): (Table, String)| {
                    *title_sink.borrow_mut() = title;
                    Ok(())
                })
                .map_err(lua_error)?,
        )
        .map_err(lua_error)?;
        tab.set(
            "close",
            self._lua
                .create_function(|_, _tab: Table| Ok(()))
                .map_err(lua_error)?,
        )
        .map_err(lua_error)?;
        let returned = callback
            .call::<LuaValue>((path.to_string_lossy().to_string(), tab.clone()))
            .map_err(lua_error)?;
        *self.last_file_title.borrow_mut() = Some(tab_title.borrow().clone());
        if let Some(close) = tab.get::<Option<Function>>("on_close").map_err(lua_error)? {
            let mut next = self.next_file_close_id.borrow_mut();
            *next = next.saturating_add(1);
            let id = *next;
            let key = self._lua.create_registry_value(close).map_err(lua_error)?;
            self.file_close_callbacks.borrow_mut().insert(id, key);
            *self.last_file_close_id.borrow_mut() = Some(id);
        }
        if let LuaValue::String(text) = returned {
            return Ok(Some(
                text.to_str()
                    .map_or_else(|_| String::new(), |text| text.to_owned()),
            ));
        }
        let Some(render) = tab.get::<Option<Function>>("render").map_err(lua_error)? else {
            return Ok(None);
        };
        let lines = Rc::new(RefCell::new(Vec::<String>::new()));
        let sink = Rc::clone(&lines);
        let panel = self._lua.create_table().map_err(lua_error)?;
        panel
            .set(
                "label",
                self._lua
                    .create_function(move |_, (_panel, value): (Table, String)| {
                        sink.borrow_mut().push(value);
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        panel
            .set(
                "text",
                self._lua
                    .create_function(|_, (_panel, _value): (Table, String)| Ok(()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for (name, table_mode) in [("list", false), ("table", true)] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                            if let Some(value) = value {
                                sink.borrow_mut()
                                    .push(collection_widget_text(lua, value, table_mode));
                            }
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let pager_sink = Rc::clone(&lines);
        panel
            .set(
                "pager",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            let page = value.get::<Option<u64>>("page").ok().flatten();
                            let total = value
                                .get::<Option<u64>>("pages")
                                .ok()
                                .flatten()
                                .or_else(|| value.get::<Option<u64>>("total").ok().flatten());
                            if let (Some(page), Some(total)) = (page, total) {
                                pager_sink.borrow_mut().push(format!("Page {page}/{total}"));
                            } else {
                                pager_sink
                                    .borrow_mut()
                                    .push(lua_value_text(lua, LuaValue::Table(value)));
                            }
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for name in ["tabs", "scrollview"] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                            if let Some(value) = value {
                                sink.borrow_mut()
                                    .push(collection_widget_text(lua, value, false));
                            }
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        for name in [
            "badge",
            "border",
            "dialog",
            "dropdown",
            "hstack",
            "password",
            "raw",
            "row_action",
            "title",
            "vstack",
        ] {
            let sink = Rc::clone(&lines);
            panel
                .set(
                    name,
                    self._lua
                        .create_function(
                            move |lua, (_panel, values): (Table, Variadic<LuaValue>)| {
                                if !values.is_empty() {
                                    sink.borrow_mut().push(
                                        values
                                            .into_iter()
                                            .map(|value| lua_value_text(lua, value))
                                            .collect::<Vec<_>>()
                                            .join(" "),
                                    );
                                }
                                Ok(())
                            },
                        )
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let rendered = render.call::<LuaValue>(panel).map_err(lua_error)?;
        if let LuaValue::String(text) = rendered {
            lines.borrow_mut().push(
                text.to_str()
                    .map_or_else(|_| String::new(), |text| text.to_owned()),
            );
        }
        let text = lines.borrow().join("\n");
        Ok(Some(text))
    }

    pub fn take_file_close_id(&self) -> Option<u64> {
        self.last_file_close_id.borrow_mut().take()
    }

    pub fn take_file_title(&self) -> Option<String> {
        self.last_file_title.borrow_mut().take()
    }

    pub fn close_file(&self, id: u64) -> AppResult<()> {
        let Some(key) = self.file_close_callbacks.borrow_mut().remove(&id) else {
            return Ok(());
        };
        let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
        callback.call::<()>(()).map_err(lua_error)
    }

    pub fn close_tab(&self, id: &str) -> AppResult<bool> {
        let callback = self.open_tabs.borrow_mut().remove(id).flatten();
        let Some(key) = callback else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
        callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("tab on_close failed: {error}")))?;
        Ok(true)
    }
}

impl Drop for PluginTerminalManager {
    fn drop(&mut self) {
        for mut session in std::mem::take(&mut self.sessions).into_values() {
            let _ = session.child.kill();
            let _ = session.child.wait();
        }
    }
}

fn create_terminal_module(
    lua: &Lua,
    can_control: bool,
    can_read: bool,
    manager: Rc<RefCell<PluginTerminalManager>>,
) -> AppResult<Table> {
    let module = lua.create_table().map_err(lua_error)?;
    if can_control {
        let spawn_manager = Rc::clone(&manager);
        module
            .set(
                "spawn",
                lua.create_function(move |_, spec: Table| {
                    let shell = spec
                        .get::<Option<String>>("shell")?
                        .filter(|value| !value.is_empty())
                        .or_else(|| std::env::var("SHELL").ok())
                        .unwrap_or_else(|| "/bin/sh".to_owned());
                    let args = spec
                        .get::<Option<Table>>("args")?
                        .map(|table| {
                            table
                                .sequence_values::<String>()
                                .collect::<mlua::Result<Vec<_>>>()
                        })
                        .transpose()?
                        .unwrap_or_default();
                    let cwd = spec.get::<Option<String>>("cwd")?;
                    let label = spec
                        .get::<Option<String>>("label")?
                        .unwrap_or_else(|| shell.clone());
                    let pty_system = native_pty_system();
                    let pair = pty_system
                        .openpty(PtySize {
                            rows: 24,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .map_err(mlua::Error::external)?;
                    let mut command = CommandBuilder::new(&shell);
                    command.args(args);
                    if let Some(cwd) = cwd {
                        command.cwd(cwd);
                    }
                    if let Some(env) = spec.get::<Option<Table>>("env")? {
                        for pair in env.pairs::<String, String>() {
                            let (key, value) = pair?;
                            command.env(key, value);
                        }
                    }
                    let child = pair
                        .slave
                        .spawn_command(command)
                        .map_err(mlua::Error::external)?;
                    drop(pair.slave);
                    let master = pair.master;
                    let mut reader = master.try_clone_reader().map_err(mlua::Error::external)?;
                    let writer = master.take_writer().map_err(mlua::Error::external)?;
                    let output = Arc::new(Mutex::new(Vec::new()));
                    let screen = Arc::new(Mutex::new(TerminalScreen::new(80, 24)));
                    let sink = Arc::clone(&output);
                    let screen_sink = Arc::clone(&screen);
                    thread::spawn(move || {
                        let mut bytes = [0_u8; 4096];
                        loop {
                            match reader.read(&mut bytes) {
                                Ok(0) | Err(_) => break,
                                Ok(count) => {
                                    if let Ok(mut output) = sink.lock() {
                                        append_terminal_output(&mut output, &bytes[..count]);
                                    }
                                    if let Ok(mut screen) = screen_sink.lock() {
                                        screen.feed(&bytes[..count]);
                                    }
                                }
                            }
                        }
                    });
                    let mut manager = spawn_manager.borrow_mut();
                    manager.next_id = manager.next_id.saturating_add(1);
                    let id = format!("plugin-terminal-{}", manager.next_id);
                    manager.sessions.insert(
                        id.clone(),
                        PluginTerminalSession {
                            label,
                            _master: master,
                            writer,
                            child,
                            output,
                            screen,
                            columns: 80,
                            rows: 24,
                        },
                    );
                    Ok(id)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let send_manager = Rc::clone(&manager);
        module
            .set(
                "send",
                lua.create_function(move |_, (id, text): (String, String)| {
                    let mut manager = send_manager.borrow_mut();
                    let session = manager
                        .sessions
                        .get_mut(&id)
                        .ok_or_else(|| mlua::Error::external("unknown terminal id"))?;
                    session
                        .writer
                        .write_all(text.as_bytes())
                        .map_err(mlua::Error::external)?;
                    session.writer.flush().map_err(mlua::Error::external)?;
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let resize_manager = Rc::clone(&manager);
        module
            .set(
                "resize",
                lua.create_function(move |_, (id, columns, rows): (String, u16, u16)| {
                    let mut manager = resize_manager.borrow_mut();
                    let session = manager
                        .sessions
                        .get_mut(&id)
                        .ok_or_else(|| mlua::Error::external("unknown terminal id"))?;
                    let columns = columns.max(1);
                    let rows = rows.max(1);
                    session
                        ._master
                        .resize(PtySize {
                            rows,
                            cols: columns,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .map_err(mlua::Error::external)?;
                    session.columns = columns;
                    session.rows = rows;
                    session
                        .screen
                        .lock()
                        .map_err(|_| mlua::Error::external("terminal screen lock poisoned"))?
                        .resize(columns, rows);
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let focus_manager = Rc::clone(&manager);
        module
            .set(
                "focus",
                lua.create_function(move |_, id: String| {
                    if focus_manager.borrow().sessions.contains_key(&id) {
                        Ok(true)
                    } else {
                        Err(mlua::Error::external("unknown terminal id"))
                    }
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let close_manager = Rc::clone(&manager);
        module
            .set(
                "close",
                lua.create_function(move |_, id: String| {
                    let Some(mut session) = close_manager.borrow_mut().sessions.remove(&id) else {
                        return Err(mlua::Error::external("unknown terminal id"));
                    };
                    let _ = session.child.kill();
                    let _ = session.child.wait();
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
    }
    if can_read {
        let read_manager = Rc::clone(&manager);
        module
            .set(
                "read",
                lua.create_function(move |_, (id, max_bytes): (String, Option<usize>)| {
                    let manager = read_manager.borrow();
                    let session = manager
                        .sessions
                        .get(&id)
                        .ok_or_else(|| mlua::Error::external("unknown terminal id"))?;
                    let output = session
                        .output
                        .lock()
                        .map_err(|_| mlua::Error::external("terminal output lock poisoned"))?;
                    let start = max_bytes.map_or(0, |limit| output.len().saturating_sub(limit));
                    Ok(String::from_utf8_lossy(&output[start..]).into_owned())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let screen_manager = Rc::clone(&manager);
        module
            .set(
                "screen",
                lua.create_function(move |_, id: String| {
                    let manager = screen_manager.borrow();
                    let session = manager
                        .sessions
                        .get(&id)
                        .ok_or_else(|| mlua::Error::external("unknown terminal id"))?;
                    let output = session
                        .output
                        .lock()
                        .map_err(|_| mlua::Error::external("terminal output lock poisoned"))?;
                    let mut screen = TerminalScreen::new(session.columns, session.rows);
                    screen.feed(&output);
                    Ok(screen.lines().join("\n"))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let list_manager = Rc::clone(&manager);
        module
            .set(
                "list",
                lua.create_function(move |lua, ()| {
                    let manager = list_manager.borrow();
                    let result = lua.create_table()?;
                    for (index, (id, session)) in manager.sessions.iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("id", id.clone())?;
                        item.set("label", session.label.clone())?;
                        item.set("cols", session.columns)?;
                        item.set("rows", session.rows)?;
                        result.set(index + 1, item)?;
                    }
                    Ok(result)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
    }
    Ok(module)
}

#[derive(Debug, Default)]
struct CommandOptions {
    stdin: Option<String>,
    env: Vec<(String, String)>,
    timeout_ms: Option<u64>,
}

fn parse_command_values(values: Vec<LuaValue>) -> mlua::Result<(Vec<String>, CommandOptions)> {
    let mut values = values.into_iter();
    let command = values
        .next()
        .ok_or_else(|| mlua::Error::external("missing command"))?;
    let args = match values.next() {
        Some(LuaValue::Table(table)) => Some(table),
        Some(other) => {
            return Err(mlua::Error::external(format!(
                "arguments must be a table, got {other:?}"
            )))
        }
        None => None,
    };
    let options = match values.next() {
        Some(LuaValue::Table(table)) => {
            let stdin = table.get::<Option<String>>("stdin")?;
            let timeout_ms = table.get::<Option<u64>>("timeout_ms")?;
            let env = table
                .get::<Option<Table>>("env")?
                .map(|table| {
                    table
                        .pairs::<String, String>()
                        .collect::<mlua::Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default();
            CommandOptions {
                stdin,
                env,
                timeout_ms,
            }
        }
        Some(other) => {
            return Err(mlua::Error::external(format!(
                "options must be a table, got {other:?}"
            )))
        }
        None => CommandOptions::default(),
    };
    if values.next().is_some() {
        return Err(mlua::Error::external("too many command arguments"));
    }
    let argv = command_argv(command, args)?;
    Ok((argv, options))
}

fn command_argv(command: LuaValue, args: Option<Table>) -> mlua::Result<Vec<String>> {
    let mut argv = match command {
        LuaValue::String(command) => vec![command
            .to_str()
            .map_err(|error| mlua::Error::external(error.to_string()))?
            .to_owned()],
        LuaValue::Table(spec) => {
            let program = spec
                .get::<String>("command")
                .or_else(|_| spec.get("shell"))?;
            vec![program]
        }
        _ => return Err(mlua::Error::external("command must be a string or table")),
    };
    if let Some(args) = args {
        for index in 1..=args.raw_len() {
            argv.push(args.get(index)?);
        }
    }
    Ok(argv)
}

fn run_command(
    argv: &[String],
    options: &CommandOptions,
) -> mlua::Result<(std::process::Output, bool)> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| mlua::Error::external("empty command"))?;
    let mut command = Command::new(program);
    command.args(rest);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if options.stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    command.envs(options.env.iter().map(|(key, value)| (key, value)));
    let mut child = command.spawn().map_err(mlua::Error::external)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| mlua::Error::external("stdout pipe unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| mlua::Error::external("stderr pipe unavailable"))?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stdin_writer = options.stdin.as_deref().and_then(|stdin| {
        child.stdin.take().map(|mut pipe| {
            let stdin = stdin.as_bytes().to_owned();
            thread::spawn(move || pipe.write_all(&stdin))
        })
    });
    let deadline = options
        .timeout_ms
        .map(|milliseconds| Instant::now() + Duration::from_millis(milliseconds));
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(mlua::Error::external)? {
            break status;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            child.kill().map_err(mlua::Error::external)?;
            break child.wait().map_err(mlua::Error::external)?;
        }
        thread::sleep(Duration::from_millis(5));
    };
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| mlua::Error::external("stdout reader panicked"))?
        .map_err(mlua::Error::external)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| mlua::Error::external("stderr reader panicked"))?
        .map_err(mlua::Error::external)?;
    Ok((
        std::process::Output {
            status,
            stdout,
            stderr,
        },
        timed_out,
    ))
}

fn command_result(lua: &Lua, output: std::process::Output, timed_out: bool) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set(
        "stdout",
        String::from_utf8_lossy(&output.stdout).to_string(),
    )?;
    result.set(
        "stderr",
        String::from_utf8_lossy(&output.stderr).to_string(),
    )?;
    result.set("exit_code", output.status.code().unwrap_or(-1))?;
    result.set("ok", output.status.success())?;
    result.set("timed_out", timed_out)?;
    Ok(result)
}

fn lua_value_text(lua: &Lua, value: LuaValue) -> String {
    match value {
        LuaValue::String(text) => text
            .to_str()
            .map_or_else(|_| String::new(), |text| text.to_owned()),
        LuaValue::Integer(value) => value.to_string(),
        LuaValue::Number(value) => value.to_string(),
        LuaValue::Boolean(value) => value.to_string(),
        other => lua
            .from_value::<JsonValue>(other)
            .map(|value| value.to_string())
            .unwrap_or_default(),
    }
}

fn collection_widget_text(lua: &Lua, value: LuaValue, table_mode: bool) -> String {
    let LuaValue::Table(table) = value else {
        return lua_value_text(lua, value);
    };
    if table.get::<Option<bool>>("loading").ok().flatten() == Some(true) {
        return "⏳ loading".to_owned();
    }
    let key = if table_mode { "rows" } else { "items" };
    let Ok(items) = table.get::<Option<Table>>(key) else {
        return lua_value_text(lua, LuaValue::Table(table));
    };
    let Some(items) = items else {
        return table
            .get::<Option<String>>("empty")
            .ok()
            .flatten()
            .unwrap_or_default();
    };
    let selected = table
        .get::<Option<u64>>("selected")
        .ok()
        .flatten()
        .and_then(|value| value.checked_sub(1))
        .map(|value| value as usize);
    let values: Vec<_> = items.sequence_values::<LuaValue>().flatten().collect();
    if values.is_empty() {
        return table
            .get::<Option<String>>("empty")
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let text = collection_item_text(lua, value, table_mode);
            if selected == Some(index) {
                format!("▶ {text}")
            } else {
                format!("  {text}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn markdown_to_lua(lua: &Lua, markdown: &str) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    for (line_index, line) in markdown.lines().enumerate() {
        let spans = lua.create_table()?;
        let (style, content) = if let Some(content) = line.strip_prefix("# ") {
            ("heading", content)
        } else if let Some(content) = line.strip_prefix("## ") {
            ("heading", content)
        } else if let Some(content) = line.strip_prefix("### ") {
            ("heading", content)
        } else {
            ("default", line)
        };
        if style != "default" {
            let span = lua.create_table()?;
            span.set("text", content)?;
            span.set("style", style)?;
            spans.set(1, span)?;
        } else {
            let parsed = markdown_inline_spans(content);
            for (index, (text, style)) in parsed.into_iter().enumerate() {
                let span = lua.create_table()?;
                span.set("text", text)?;
                span.set("style", style)?;
                spans.set(index + 1, span)?;
            }
        }
        result.set(line_index + 1, spans)?;
    }
    Ok(result)
}

fn markdown_inline_spans(line: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut index = 0;
    let flush = |spans: &mut Vec<(String, String)>, plain: &mut String| {
        if !plain.is_empty() {
            spans.push((std::mem::take(plain), "default".to_owned()));
        }
    };
    while index < chars.len() {
        let (delimiter, style, width) =
            if index + 1 < chars.len() && chars[index] == '*' && chars[index + 1] == '*' {
                ("**", "bold", 2)
            } else if chars[index] == '`' {
                ("`", "code", 1)
            } else if chars[index] == '*' || chars[index] == '_' {
                (if chars[index] == '_' { "_" } else { "*" }, "italic", 1)
            } else {
                plain.push(chars[index]);
                index += 1;
                continue;
            };
        let mut end = None;
        let mut cursor = index + width;
        while cursor + width <= chars.len() {
            let matches = if width == 2 {
                chars[cursor] == '*' && chars[cursor + 1] == '*'
            } else {
                chars[cursor] == delimiter.chars().next().unwrap_or('`')
            };
            if matches {
                end = Some(cursor);
                break;
            }
            cursor += 1;
        }
        let Some(end) = end else {
            plain.push(chars[index]);
            index += 1;
            continue;
        };
        flush(&mut spans, &mut plain);
        spans.push((chars[index + width..end].iter().collect(), style.to_owned()));
        index = end + width;
    }
    flush(&mut spans, &mut plain);
    spans
}

fn collection_item_text(lua: &Lua, value: LuaValue, table_mode: bool) -> String {
    let LuaValue::Table(item) = value else {
        return lua_value_text(lua, value);
    };
    let text = if table_mode {
        item.get::<Option<Table>>("cells")
            .ok()
            .flatten()
            .map(|cells| {
                cells
                    .sequence_values::<LuaValue>()
                    .flatten()
                    .map(|cell| lua_value_text(lua, cell))
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .filter(|text| !text.is_empty())
            .or_else(|| item.get::<Option<String>>("label").ok().flatten())
            .or_else(|| item.get::<Option<String>>("text").ok().flatten())
    } else {
        item.get::<Option<String>>("label")
            .ok()
            .flatten()
            .or_else(|| item.get::<Option<String>>("text").ok().flatten())
    }
    .unwrap_or_else(|| lua_value_text(lua, LuaValue::Table(item.clone())));
    item.get::<Option<String>>("right")
        .ok()
        .flatten()
        .map_or(text.clone(), |right| format!("{text}  {right}"))
}

fn lua_error(error: mlua::Error) -> crate::error::AppError {
    plugin_error(error.to_string())
}

fn parse_commands(table: Table) -> mlua::Result<Vec<PluginCommand>> {
    table
        .sequence_values::<Table>()
        .map(|item| {
            let item = item?;
            Ok(PluginCommand {
                id: item.get::<String>("id").unwrap_or_default(),
                title: item
                    .get::<Option<String>>("title")?
                    .unwrap_or_else(|| item.get::<String>("id").unwrap_or_default()),
            })
        })
        .collect()
}

fn parse_keybindings(table: Table) -> mlua::Result<Vec<PluginKeybinding>> {
    table
        .sequence_values::<Table>()
        .map(|item| {
            let item = item?;
            Ok(PluginKeybinding {
                key: item.get::<String>("key").unwrap_or_default(),
                command: item.get::<String>("command").unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_status_items(table: Table) -> mlua::Result<Vec<PluginStatusItem>> {
    table
        .sequence_values::<LuaValue>()
        .enumerate()
        .map(|(index, value)| {
            let value = value?;
            let (id, text, side, priority) = match value {
                LuaValue::String(value) => {
                    let text = value.to_str()?.to_owned();
                    (format!("status-{index}"), text, "left".to_owned(), 1000)
                }
                LuaValue::Table(item) => {
                    let id = item
                        .get::<Option<String>>("id")?
                        .unwrap_or_else(|| format!("status-{index}"));
                    let text = item
                        .get::<Option<String>>("text")?
                        .or(item.get::<Option<String>>("label")?)
                        .unwrap_or_else(|| id.clone());
                    let side = item
                        .get::<Option<String>>("side")?
                        .unwrap_or_else(|| "left".to_owned());
                    let priority = item.get::<Option<i64>>("priority")?.unwrap_or(1000);
                    (id, text, side, priority)
                }
                _ => (
                    format!("status-{index}"),
                    String::new(),
                    "left".to_owned(),
                    1000,
                ),
            };
            Ok(PluginStatusItem {
                id,
                text,
                side,
                priority,
            })
        })
        .filter(|item| item.as_ref().map_or(true, |item| !item.text.is_empty()))
        .collect()
}

fn line_diff(old: &[String], new: &[String]) -> Vec<String> {
    if old.len() > 2048 || new.len() > 2048 {
        return old
            .iter()
            .map(|line| format!("- {line}"))
            .chain(new.iter().map(|line| format!("+ {line}")))
            .collect();
    }

    let mut lcs = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut result = Vec::with_capacity(old.len() + new.len());
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            result.push(format!("  {}", old[i]));
            i += 1;
            j += 1;
        } else if i < old.len() && (j == new.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            result.push(format!("- {}", old[i]));
            i += 1;
        } else {
            result.push(format!("+ {}", new[j]));
            j += 1;
        }
    }
    result
}

fn parse_file_handlers(table: Table) -> mlua::Result<Vec<PluginFileHandler>> {
    table
        .sequence_values::<Table>()
        .map(|item| {
            let item = item?;
            let id = item.get::<String>("id").unwrap_or_default();
            let title = item
                .get::<Option<String>>("title")?
                .unwrap_or_else(|| id.clone());
            let extensions = item
                .get::<Option<Table>>("extensions")?
                .map(|extensions| {
                    extensions
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default();
            Ok(PluginFileHandler {
                id,
                title,
                extensions,
            })
        })
        .collect()
}

fn parse_panel(table: &Table, location: PluginPanelLocation) -> mlua::Result<Option<PluginPanel>> {
    let title = table
        .get::<Option<String>>("title")?
        .unwrap_or_else(|| "Plugin panel".to_owned());
    let id = table
        .get::<Option<String>>("id")?
        .unwrap_or_else(|| title.to_lowercase().replace([' ', '/'], "-"));
    Ok(Some(PluginPanel {
        id,
        title,
        location,
    }))
}

fn default_api() -> u32 {
    1
}

fn deserialize_permissions<'de, D>(deserializer: D) -> Result<BTreeMap<String, bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = BTreeMap::<String, JsonValue>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|(name, value)| (name, value.as_bool().unwrap_or(true)))
        .collect())
}
fn plugin_error(message: impl Into<String>) -> crate::error::AppError {
    crate::error::AppError::Plugin(message.into())
}

fn connect_rpc_transport(endpoint: &str) -> Result<RpcTransport, String> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            let path = if let Some(path) = path.strip_prefix("~/") {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map_or_else(|| PathBuf::from(path), |home| home.join(path))
            } else {
                PathBuf::from(path)
            };
            return std::os::unix::net::UnixStream::connect(path)
                .map(RpcTransport::Unix)
                .map_err(|error| format!("RPC connect failed: {error}"));
        }
        #[cfg(not(unix))]
        {
            return Err("unix RPC endpoints are unsupported on this platform".to_owned());
        }
    }
    let address = endpoint.strip_prefix("tcp://").unwrap_or(endpoint);
    std::net::TcpStream::connect(address)
        .map(RpcTransport::Tcp)
        .map_err(|error| format!("RPC connect failed: {error}"))
}

fn write_rpc_message(stream: &mut RpcTransport, message: &JsonValue) -> Result<(), String> {
    let mut encoded = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    if encoded.len() > 32 * 1024 * 1024 {
        return Err("RPC message exceeds 32 MiB".to_owned());
    }
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

fn spawn_rpc_reader(connection_id: u64, stream: RpcTransport, events: Sender<RpcEvent>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line);
            if read.is_err() || read.ok() == Some(0) {
                let _ = events.send(RpcEvent {
                    connection_id,
                    kind: RpcEventKind::Closed,
                });
                break;
            }
            let Ok(message) = serde_json::from_str::<JsonValue>(&line) else {
                continue;
            };
            if let Some(id) = message.get("id").and_then(JsonValue::as_u64) {
                if message.get("method").is_none() {
                    let _ = events.send(RpcEvent {
                        connection_id,
                        kind: RpcEventKind::Response { id, message },
                    });
                    continue;
                }
                let method = message
                    .get("method")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let params = message.get("params").cloned().unwrap_or(JsonValue::Null);
                let _ = events.send(RpcEvent {
                    connection_id,
                    kind: RpcEventKind::Request { id, method, params },
                });
            } else if let Some(method) = message.get("method").and_then(JsonValue::as_str) {
                let params = message.get("params").cloned().unwrap_or(JsonValue::Null);
                let _ = events.send(RpcEvent {
                    connection_id,
                    kind: RpcEventKind::Notification {
                        method: method.to_owned(),
                        params,
                    },
                });
            }
        }
    });
}

fn ensure_inside(root: &Path, relative: &str) -> AppResult<()> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(plugin_error(format!(
            "plugin path escapes package: {relative}"
        )));
    }
    let candidate = root.join(path);
    if !candidate.starts_with(root) {
        return Err(plugin_error(format!(
            "plugin path escapes package: {relative}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_diff_preserves_context_and_order() {
        let old = vec!["same".to_owned(), "old".to_owned(), "tail".to_owned()];
        let new = vec!["same".to_owned(), "new".to_owned(), "tail".to_owned()];
        assert_eq!(
            line_diff(&old, &new),
            vec!["  same", "- old", "+ new", "  tail"]
        );
    }

    #[test]
    fn terminal_output_buffer_keeps_bounded_tail() {
        let mut output = Vec::new();
        append_terminal_output(&mut output, b"prefix");
        append_terminal_output(&mut output, &vec![b'x'; PLUGIN_TERMINAL_OUTPUT_LIMIT + 8]);
        assert_eq!(output.len(), PLUGIN_TERMINAL_OUTPUT_LIMIT);
        assert!(output.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn parses_ttt_manifest() {
        let json = r#"{"name":"hello","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true,"system.exec":["git"],"fs.read":["{workspace}"]}}"#;
        let m: Manifest = serde_json::from_str(json).expect("manifest");
        assert_eq!(m.title(), "hello");
        assert!(m.permissions["panel.sidebar"]);
        assert!(m.permissions["system.exec"]);
        assert!(m.permissions["fs.read"]);
    }

    #[test]
    fn editor_read_api_exposes_snapshot_with_one_based_positions() {
        let root = std::env::temp_dir().join(format!("mtide-editor-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"editor-api","entry":"init.lua","api":2,"permissions":{"editor.read":true,"editor.write":true,"editor.diagnostics":true,"editor.decorations":true,"editor.bookmarks":true,"screen.read":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
local editor = require("ttt.editor")
local events = require("ttt.events")
local diagnostics = require("ttt.diagnostics")
local decorations = require("ttt.decorations")
local bookmarks = require("ttt.bookmarks")
local automation = require("ttt.automation")
local rpc = require("ttt.rpc")
assert(type(rpc.connect) == "function")
ttt.on("probe", function()
  assert(editor.buffer_text() == "one\ntwo")
  assert(editor.line_count() == 2)
  assert(editor.current_line() == "two")
  assert(editor.get_line(1) == "one")
  assert(editor.file_path() == "/tmp/demo.rs")
  assert(editor.file_name() == "demo.rs")
  assert(editor.language() == "rust")
  assert(type(ttt.executable_path()) == "string" and #ttt.executable_path() > 0)
  local graphics = ttt.graphics()
  assert(graphics.protocol == "none" and graphics.cell_w == 0 and graphics.cell_h == 0)
  assert(ttt.screenshot():find("one", 1, true) ~= nil)
  assert(ttt.debug():find("demo.rs", 1, true) ~= nil)
  assert(editor.byte_to_col("你a", 3) == 2)
  assert(editor.col_to_byte("你a", 2) == 3)
  assert(editor.viewport().top_line == 1)
  assert(editor.scroll_to(2) == true)
  assert(editor.scroll_by(1) == true)
  assert(editor.set_search("one") == true)
  assert(editor.set_search("o.e", true) == true)
  assert(editor.clear_search() == true)
  local cursor = editor.cursor()
  assert(cursor.line == 2 and cursor.col == 3)
  local selection = editor.selection()
  assert(selection.active and selection.start_line == 1 and selection.end_col == 3)
  assert(editor.selection_text() == "one\ntw")
  assert(editor.begin_undo_group() == true)
  assert(editor.insert(2, 3, "!") == true)
  assert(editor.set_line(1, "ONE") == true)
  assert(editor.replace(1, 1, 1, 2, "O") == true)
  assert(editor.set_cursor(2, 1) == true)
  assert(editor.set_selection(1, 1, 2, 2) == true)
  assert(editor.clear_selection() == true)
  assert(editor.add_cursor(1, 2) == true)
  assert(editor.clear_cursors() == true)
  assert(editor.end_undo_group() == true)
  assert(diagnostics.publish("/tmp/demo.rs", {{line = 2, col = 1, end_line = 2, end_col = 3, severity = "error", message = "bad line", source = "test"}}) == true)
  assert(decorations.set("/tmp/decor.rs", {{line = 3, end_line = 4, style = "hint", note = "decorated"}}) == true)
  assert(bookmarks.set(2, "★", "success") == true)
  local mark = bookmarks.get(2)
  assert(mark.icon == "★" and mark.style == "success")
  assert(#bookmarks.get_all() == 1)
  assert(bookmarks.remove(2) == true)
  assert(bookmarks.set_all({{line = 1, icon = "◆", style = "bookmark"}}) == true)
end)
events.on("probe", function() ttt.log("events-alias") end)
events.on("payload", function(path, line, col, language, selection_active, selection_text)
  assert(path == "/tmp/demo.rs" and line == 2 and col == 3)
  assert(language == "rust" and selection_active == true and selection_text == "two")
  ttt.log("payload-ok")
end)
local timer = ttt.set_timeout(0, function() ttt.log("timer-ok") end)
local interval = ttt.set_interval(60000, function() ttt.log("interval") end)
assert(ttt.clear_interval(interval) == true)
automation.run("buffer", "", function(result, err)
  assert(err == nil and result.ok == true)
  ttt.log("automation-ok")
end)
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.set_editor_snapshot(PluginEditorSnapshot {
            lines: vec!["one".into(), "two".into()],
            path: "/tmp/demo.rs".into(),
            file_name: "demo.rs".into(),
            language: "rust".into(),
            cursor_line: 1,
            cursor_col: 2,
            cursors: vec![(1, 2)],
            selection: Some((0, 0, 1, 2)),
            selection_text: "one\ntw".into(),
        });
        runtime.poll_async().expect("timer");
        assert!(runtime.logs().iter().any(|line| line == "[info] timer-ok"));
        let tasks = runtime.drain_automation_requests();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].action, "buffer");
        assert!(runtime
            .complete_automation(tasks[0].id, serde_json::json!({"ok": true}), None)
            .expect("automation"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] automation-ok"));
        assert!(runtime.emit("probe").expect("probe"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] events-alias"));
        assert!(runtime
            .emit_with_payload(
                "payload",
                &[
                    PluginEventArg::Text("/tmp/demo.rs".into()),
                    PluginEventArg::Number(2),
                    PluginEventArg::Number(3),
                    PluginEventArg::Text("rust".into()),
                    PluginEventArg::Bool(true),
                    PluginEventArg::Text("two".into()),
                ],
            )
            .expect("payload"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] payload-ok"));
        assert_eq!(runtime.diagnostic_severity("/tmp/demo.rs", 1), Some(1));
        assert_eq!(runtime.diagnostic_severity("/tmp/decor.rs", 2), Some(3));
        assert_eq!(
            runtime.diagnostic_message("/tmp/decor.rs", 2).as_deref(),
            Some("decorated")
        );
        assert_eq!(
            runtime.diagnostic_message("/tmp/demo.rs", 1).as_deref(),
            Some("bad line")
        );
        assert_eq!(
            runtime.bookmark_marker("/tmp/demo.rs", 0).as_deref(),
            Some("◆")
        );
        assert_eq!(runtime.drain_bookmark_events().len(), 3);
        assert_eq!(
            runtime.drain_editor_requests(),
            vec![
                PluginEditorRequest::ScrollTo { line: 1 },
                PluginEditorRequest::ScrollBy { delta: 1 },
                PluginEditorRequest::SetSearch {
                    pattern: "one".into(),
                    use_regex: false,
                },
                PluginEditorRequest::SetSearch {
                    pattern: "o.e".into(),
                    use_regex: true,
                },
                PluginEditorRequest::ClearSearch,
                PluginEditorRequest::BeginUndoGroup,
                PluginEditorRequest::Insert {
                    line: 1,
                    col: 2,
                    text: "!".into()
                },
                PluginEditorRequest::SetLine {
                    line: 0,
                    text: "ONE".into()
                },
                PluginEditorRequest::Replace {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 1,
                    text: "O".into()
                },
                PluginEditorRequest::SetCursor { line: 1, col: 0 },
                PluginEditorRequest::SetSelection {
                    start_line: 0,
                    start_col: 0,
                    end_line: 1,
                    end_col: 1
                },
                PluginEditorRequest::ClearSelection,
                PluginEditorRequest::AddCursor { line: 0, col: 1 },
                PluginEditorRequest::ClearCursors,
                PluginEditorRequest::EndUndoGroup,
            ]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dialog_api_queues_requests_and_runs_confirm_callback() {
        let root = std::env::temp_dir().join(format!("mtide-dialog-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"dialog-api","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
ttt.confirm("Delete it?", function() ttt.log("confirmed") end)
ttt.show_info("Info", {{key = "a", value = "b"}})
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let requests = runtime.drain_panel_requests();
        assert_eq!(requests.len(), 2);
        let PluginPanelRequest::Confirm { id, message } = requests[0].clone() else {
            panic!("expected confirm request");
        };
        assert_eq!(message, "Delete it?");
        assert_eq!(
            requests[1],
            PluginPanelRequest::ShowInfo {
                title: "Info".into(),
                entries: vec![("a".into(), "b".into())]
            }
        );
        runtime.resolve_dialog(id, true).expect("resolve");
        assert!(runtime.logs().iter().any(|line| line == "[info] confirmed"));
    }

    #[test]
    fn command_line_api_requires_permission_and_dispatches_callbacks() {
        let root =
            std::env::temp_dir().join(format!("mtide-command-line-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"command-line-api","entry":"init.lua","api":2,"permissions":{"keybindings":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
local line = require("ttt.command_line")
assert(line.active() == false)
assert(line.show({prefix = "/", text = "a", on_change = function(text) ttt.log("change:" .. text) end, on_key = function(ev) return ev.key == "Ctrl-X" end, on_submit = function(text) ttt.log("submit:" .. text) end, on_cancel = function() ttt.log("cancel") end}) == true)
assert(line.active() == true)
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::CommandLineShow {
                id: 1,
                prefix: "/".into(),
                text: "a".into()
            }]
        );
        runtime.command_line_change("ab").expect("change");
        assert!(runtime
            .command_line_key("Ctrl-X", None, "ctrl")
            .expect("key"));
        runtime.command_line_finish("ab", true).expect("submit");
        assert!(runtime.logs().iter().any(|line| line == "[info] change:ab"));
        assert!(runtime.logs().iter().any(|line| line == "[info] submit:ab"));
    }

    #[test]
    fn network_api_returns_bounded_response_shape_and_async_errors() {
        let root = std::env::temp_dir().join(format!("mtide-network-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"network-api","entry":"init.lua","api":2,"permissions":{"network.http":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
local net = require("ttt.net")
local response = net.get("ftp://example.com")
assert(response.status == 0 and response.error ~= nil and response.body == "")
net.get_async("not a url", function(async_response)
  assert(async_response.status == 0 and async_response.error ~= nil)
  ttt.log("network-async-ok")
end)
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        for _ in 0..50 {
            runtime.poll_async().expect("network poll");
            if runtime
                .logs()
                .iter()
                .any(|line| line == "[info] network-async-ok")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] network-async-ok"));
    }

    #[test]
    fn network_host_allowlist_matches_host_and_optional_port() {
        let hosts = vec!["api.example.com".to_owned(), "127.0.0.1:4250".to_owned()];
        assert!(network_target_allowed(
            "https://api.example.com/data",
            &hosts
        ));
        assert!(network_target_allowed("http://127.0.0.1:4250/", &hosts));
        assert!(!network_target_allowed("http://127.0.0.1:444/data", &hosts));
        assert!(!network_target_allowed(
            "https://other.example.com/",
            &hosts
        ));
        assert!(network_target_allowed("https://anything.example/", &[]));
    }

    #[test]
    fn custom_dialog_renders_and_closes_with_callback() {
        let root = std::env::temp_dir().join(format!("mtide-custom-dialog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"custom-dialog","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
assert(ttt.open_dialog({title = "Review", width = 40, height = 8, render = function(panel) panel:label("dialog body"); panel:input({text = "old", on_change = function(text) ttt.log("input:" .. text) end}); panel:button({label = "Submit", on_click = function() ttt.log("clicked") end}) end, on_close = function() ttt.log("dialog-closed") end}) == true)
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::OpenDialog {
                id: 1,
                title: "Review".into(),
                width: 40,
                height: 8,
            }]
        );
        assert_eq!(
            runtime.render_panel("__dialog:1").expect("render"),
            Some("dialog body\n❯ old\n[ Submit ]".into())
        );
        assert!(runtime.dialog_input(1, "new".into()).expect("input"));
        assert!(runtime.dialog_focus_next(1));
        assert!(!runtime.dialog_append(1, 'x').expect("button focus"));
        assert!(runtime.dialog_activate(1).expect("button"));
        assert!(runtime.dialog_focus_button(1));
        assert!(runtime.dialog_activate(1).expect("button again"));
        assert!(runtime.logs().iter().any(|line| line == "[info] input:new"));
        assert!(runtime.logs().iter().any(|line| line == "[info] clicked"));
        assert!(runtime.close_custom_dialog(1).expect("close"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] dialog-closed"));
    }

    #[test]
    fn debounce_only_delivers_latest_arguments_and_can_cancel() {
        let root = std::env::temp_dir().join(format!("mtide-debounce-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"debounce","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
local run, cancel = ttt.debounce(0, function(value) ttt.log("debounced:" .. value) end)
run("first")
run("second")
assert(cancel() == true)
run("latest")
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        for _ in 0..20 {
            runtime.poll_async().expect("timer poll");
            if runtime
                .logs()
                .iter()
                .any(|line| line == "[info] debounced:latest")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] debounced:latest"));
        assert!(!runtime
            .logs()
            .iter()
            .any(|line| line == "[info] debounced:first"));
        assert!(!runtime
            .logs()
            .iter()
            .any(|line| line == "[info] debounced:second"));
    }

    #[test]
    fn markdown_inline_parser_preserves_plain_and_styled_spans() {
        assert_eq!(
            markdown_inline_spans("hello **world** `code` *em*"),
            vec![
                ("hello ".into(), "default".into()),
                ("world".into(), "bold".into()),
                (" ".into(), "default".into()),
                ("code".into(), "code".into()),
                (" ".into(), "default".into()),
                ("em".into(), "italic".into()),
            ]
        );
        assert_eq!(
            markdown_inline_spans("_underlined_"),
            vec![("underlined".into(), "italic".into())]
        );
    }

    #[test]
    fn open_changes_creates_a_renderable_diff_tab() {
        let root = std::env::temp_dir().join(format!("mtide-open-changes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"changes","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
assert(ttt.open_changes({id = "review", title = "Review", description = "summary", diff = "diff --git a/a.txt b/a.txt\n@@ -1 +1 @@\n-old\n+new"}) == "review")
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::OpenTab {
                id: "review".into(),
                title: "Review".into(),
                focus: true,
            }]
        );
        assert_eq!(
            runtime.render_panel("__tab:review").expect("render"),
            Some("summary\ndiff --git a/a.txt b/a.txt\n@@ -1 +1 @@\n-old\n+new".into())
        );
    }

    #[test]
    fn system_spawn_streams_output_and_exposes_process_controls() {
        let root = std::env::temp_dir().join(format!("mtide-spawn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"spawn","entry":"init.lua","api":2,"permissions":{"system.exec":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
local sys = require("ttt.system")
local proc = sys.spawn({shell = "/bin/sh", args = {"-c", "printf spawned; printf warning >&2"}, on_stdout = function(chunk) ttt.log("out:" .. chunk) end, on_stderr = function(chunk) ttt.log("err:" .. chunk) end, on_exit = function(code) ttt.log("exit:" .. code) end})
assert(proc:pid() > 0)
assert(proc:close_stdin() == true)
local input_proc = sys.spawn({shell = "/bin/sh", args = {"-c", "cat"}, on_stdout = function(chunk) ttt.log("in:" .. chunk) end})
assert(input_proc:write("typed") == true)
assert(input_proc:close_stdin() == true)
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        for _ in 0..1_000 {
            runtime.poll_async().expect("spawn poll");
            let logs = runtime.logs();
            if logs.iter().any(|line| line == "[info] out:spawned")
                && logs.iter().any(|line| line == "[info] err:warning")
                && logs.iter().any(|line| line == "[info] in:typed")
                && logs.iter().any(|line| line == "[info] exit:0")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] out:spawned"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] err:warning"));
        assert!(runtime.logs().iter().any(|line| line == "[info] in:typed"));
        assert!(runtime.logs().iter().any(|line| line == "[info] exit:0"));
    }

    fn write_rpc_message_for_test(stream: &mut impl Write, message: &JsonValue) {
        let mut encoded = serde_json::to_vec(message).expect("encode message");
        encoded.push(b'\n');
        stream.write_all(&encoded).expect("write message");
        stream.flush().expect("flush message");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires local socket permission; run explicitly in an unrestricted environment"]
    fn rpc_loopback_routes_response_and_notification_on_main_poll() {
        let socket_path =
            std::env::temp_dir().join(format!("mtide-rpc-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("listener");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("request");
            let request: JsonValue = serde_json::from_str(&line).expect("json request");
            let id = request.get("id").and_then(JsonValue::as_u64).expect("id");
            let response = json!({"jsonrpc":"2.0","id":id,"result":{"answer":42}});
            let notification = json!({"jsonrpc":"2.0","method":"notice","params":{"ok":true}});
            write_rpc_message_for_test(&mut stream, &response);
            write_rpc_message_for_test(&mut stream, &notification);
        });
        let root = std::env::temp_dir().join(format!("mtide-rpc-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"rpc","entry":"init.lua","api":2,"permissions":{"network.rpc":true}}"#,
        )
        .expect("manifest");
        fs::write(root.join("init.lua"), {
            let endpoint = format!("unix:{}", socket_path.display());
            format!(
                r#"
local ttt = require("ttt")
local rpc = require("ttt.rpc")
local response_seen = false
local notification_seen = false
rpc.connect("{endpoint}", {{
  on_notification = function(method, params)
    assert(method == "notice" and params.ok == true)
    notification_seen = true
    ttt.log("rpc-notification")
  end,
}}, function(channel, err)
  assert(err == nil)
  local cancel = channel:request("hello", {{}}, function(result, request_err)
    assert(request_err == nil and result.answer == 42)
    response_seen = true
    ttt.log("rpc-response")
  end)
  assert(type(cancel) == "function")
end)
"#,
                endpoint = endpoint
            )
        })
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        for _ in 0..100 {
            runtime.poll_async().expect("poll rpc");
            if runtime
                .logs()
                .iter()
                .any(|line| line == "[info] rpc-response")
                && runtime
                    .logs()
                    .iter()
                    .any(|line| line == "[info] rpc-notification")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] rpc-response"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] rpc-notification"));
        server.join().expect("server");
        let _ = fs::remove_file(socket_path);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_parent_escape() {
        let m = Manifest {
            name: "x".into(),
            display_name: None,
            description: String::new(),
            version: String::new(),
            author: String::new(),
            entry: Some("../init.lua".into()),
            api: 1,
            permissions: BTreeMap::new(),
            network_http_hosts: Vec::new(),
            grammars: Vec::new(),
            themes: Vec::new(),
        };
        assert!(m.validate(Path::new("/tmp/plugin")).is_err());
    }

    #[test]
    fn accepts_theme_only_manifests_and_validates_theme_paths() {
        let root = std::env::temp_dir().join(format!("mtide-theme-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("themes")).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"theme-pack","themes":["themes/dark.json",{"path":"themes/light.json","label":"Light"}]}"#,
        )
        .expect("manifest");
        let package = PluginPackage::load(&root).expect("theme package");
        assert_eq!(
            package.theme_paths(),
            vec![
                root.join("themes/dark.json"),
                root.join("themes/light.json")
            ]
        );
        assert!(matches!(
            &package.manifest.themes[1],
            ThemeContribution::Descriptor { label: Some(label), .. } if label == "Light"
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejects_registration_without_declared_capability() {
        let root =
            std::env::temp_dir().join(format!("mtide-permission-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"restricted","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({commands = {{id = "restricted.run", title = "Run", handler = function() end}}})"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let error = PluginRuntime::load(&package).expect_err("registration must be denied");
        assert!(error.to_string().contains("permission denied: commands"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn loads_ttt_register_and_log_api() {
        let root = std::env::temp_dir().join(format!("mtide-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"demo","entry":"init.lua","api":2,"permissions":{"commands":true,"keybindings":true,"panel.sidebar":true,"panel.bottom":true,"panel.drawer":true,"panel.editor":true,"editor.file_handlers":true,"clipboard.read":true,"fs.read":true,"fs.write":true,"system.exec":true,"terminal.control":true,"terminal.read":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local json = require("ttt.json")
local system = require("ttt.system")
local terminal = require("ttt.terminal")
local text = require("ttt.text")
local fs = require("ttt.fs")
assert(type(ttt.version()) == "string")
assert(json.decode(json.encode({ok = true})).ok == true)
assert(text.width("你好") == 4)
assert(text.width("🙂") == 2)
assert(text.truncate("abcdef", 4) == "abc…")
assert(text.truncate("你好世界", 5) == "你好…")
assert(type(terminal.spawn) == "function")
assert(type(terminal.send) == "function")
assert(type(terminal.resize) == "function")
assert(type(terminal.read) == "function")
assert(type(terminal.screen) == "function")
assert(type(terminal.list) == "function")
local terminal_id = terminal.spawn({shell = "/bin/sh", args = {"-c", "printf plugin-terminal; sleep 1"}, label = "test"})
system.exec("/bin/sleep", {"0.5"})
assert(type(terminal.screen(terminal_id)) == "string")
assert(terminal.resize(terminal_id, 100, 30) == true)
assert(terminal.list()[1].cols == 100)
assert(terminal.focus(terminal_id) == true)
assert(#terminal.list() == 1)
local command_result = system.exec("/bin/echo", {"ok"})
assert(command_result.exit_code == 0)
ttt.log(command_result.stdout)
local env_result = system.exec("/bin/sh", {"-c", "printf $MTIDE_TEST"}, {env = {MTIDE_TEST = "from-env"}})
assert(env_result.stdout == "from-env")
local timeout_result = system.exec("/bin/sh", {"-c", "sleep 1"}, {timeout_ms = 20})
assert(timeout_result.timed_out == true)
local async_id = system.exec_async("/bin/echo", {"async"}, function(result)
  ttt.log("async:" .. result.stdout)
end)
assert(async_id > 0)
fs.write("cache.txt", "ok")
assert(fs.read("cache.txt") == "ok")
ttt.log("hello")
ttt.log("terminal-output:" .. terminal.read(terminal_id))
assert(terminal.close(terminal_id) == true)
local interactive_id = terminal.spawn({shell = "/bin/sh", args = {"-c", "read value; printf got:$value; sleep 1"}, label = "interactive"})
assert(terminal.send(interactive_id, "hello\n") == true)
system.exec("/bin/sleep", {"0.1"})
assert(string.find(terminal.read(interactive_id), "got:hello") ~= nil)
assert(terminal.close(interactive_id) == true)
ttt.on("test.event", function() assert(ttt.close_drawer() == true); assert(ttt.close_tab("demo-tab") == true); ttt.log("event-ran") end)
ttt.on("tab.open", function() ttt.log("tab-open") end)
ttt.on("tab.activate", function() ttt.log("tab-activate") end)
ttt.on("tab.close", function() ttt.log("tab-close") end)
ttt.register({sidebar = {title = "Demo", render = function(panel) panel:label("panel text"); panel:list({items = {"one", "two"}, selected = 2}); panel:table({rows = {{cells = {"row", "value"}, right = "R"}}}); panel:tabs({items = {"A", "B"}, selected = 2}); panel:pager({page = 2, pages = 4}); panel:scrollview({items = {"scroll"}, selected = 1}); panel:dialog() end}, bottom = {id = "demo.bottom", title = "Bottom", render = function(panel) panel:label("bottom text") end}, commands = {{id = "demo.run", title = "Run", handler = function() local commands = ttt.list_commands(); assert(#commands == 2); assert(ttt.exec_command("demo.other") == true); assert(ttt.show_panel("sidebar") == true); assert(ttt.focus_panel("bottom") == true); assert(ttt.open_drawer({side = "left", width = 32, min_width = 12, render = function(panel) panel:label("drawer text") end}) == true); assert(ttt.open_tab({id = "demo-tab", title = "Demo Tab", focus = true, render = function(panel) panel:label("tab text") end, on_close = function() ttt.log("tab-closed") end}) == "demo-tab"); assert(ttt.focus_tab("demo-tab") == true); ttt.set_status_item("right", "live", "Live", {priority = 10}); ttt.remove_status_item("demo.status"); ttt.log("ran") end}, {id = "demo.other", title = "Other", handler = function() assert(ttt.open_readonly("Read-only", {"one", "two"}, "note.txt") == "Read-only"); assert(ttt.open_diff("Diff", {"old"}, {"new"}, "note.txt") == "Diff"); assert(ttt.open_file("src/main.rs", 12, true) == true); assert(ttt.clipboard_write("plugin clipboard") == true); assert(ttt.clipboard_read() == "plugin clipboard"); ttt.log("other") end}}, keybindings = {{key = "ctrl-r", command = "demo.run"}}, status_items = {{id = "demo.status", text = "Demo"}}, file_handlers = {{id = "demo.pdf", extensions = {"pdf"}, open = function(path, tab) assert(tab.id == "demo.pdf"); assert(tab.title == "demo.pdf"); tab:focus(); tab:set_title("PDF"); tab.on_close = function() ttt.log("closed") end return "view:" .. path end}, {id = "demo.view", extensions = {"view"}, open = function(_path, tab) tab.render = function(panel) panel:label("file view"); panel:list({items = {"first", "second"}, selected = 2}); panel:pager({page = 1, total = 3}) end end}}})"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line.contains("terminal-output:plugin-terminal")));
        assert!(runtime.registration().sidebar);
        assert_eq!(runtime.registration().commands.len(), 2);
        assert_eq!(runtime.registration().commands[0].id, "demo.run");
        assert_eq!(runtime.registration().keybindings[0].key, "ctrl-r");
        assert_eq!(runtime.registration().status_items[0].text, "Demo");
        assert!(runtime.registration().bottom);
        assert_eq!(
            runtime.render_panel("demo.bottom").expect("bottom panel"),
            Some("bottom text".into())
        );
        assert_eq!(runtime.registration().file_handlers[0].id, "demo.pdf");
        assert_eq!(runtime.registration().file_handlers[1].id, "demo.view");
        assert_eq!(runtime.registration().panels[0].id, "demo");
        assert_eq!(
            runtime.render_panel("demo").expect("panel"),
            Some("panel text\n  one\n▶ two\n  row  value  R\n  A\n▶ B\nPage 2/4\n▶ scroll".into())
        );
        let pdf = root.join("sample.pdf");
        fs::write(&pdf, b"%PDF-").expect("pdf");
        assert_eq!(
            runtime.open_file(&pdf).expect("file handler"),
            Some(format!("view:{}", pdf.display()))
        );
        assert_eq!(runtime.take_file_title().as_deref(), Some("PDF"));
        let close_id = runtime.take_file_close_id().expect("close callback");
        runtime.close_file(close_id).expect("close file handler");
        assert_eq!(
            runtime.logs().last().map(String::as_str),
            Some("[info] closed")
        );
        let view = root.join("sample.view");
        fs::write(&view, b"view").expect("view");
        assert_eq!(
            runtime.open_file(&view).expect("rendered file handler"),
            Some("file view\n  first\n▶ second\nPage 1/3".into())
        );
        assert_eq!(runtime.logs()[0], "[info] ok\n");
        assert_eq!(runtime.logs()[1], "[info] hello");
        for _ in 0..50 {
            runtime.poll_async().expect("poll async");
            if runtime
                .logs()
                .iter()
                .any(|line| line == "[info] async:async\n")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] async:async\n"));
        assert!(runtime.execute_command("demo.run").expect("command"));
        assert_eq!(
            runtime.logs().last().map(String::as_str),
            Some("[info] ran")
        );
        assert_eq!(runtime.registration().status_items.len(), 1);
        assert_eq!(runtime.registration().status_items[0].id, "live");
        assert_eq!(runtime.registration().status_items[0].text, "Live");
        assert_eq!(runtime.registration().status_items[0].side, "right");
        assert_eq!(runtime.registration().status_items[0].priority, 10);
        assert_eq!(
            runtime.render_panel("__drawer").expect("drawer panel"),
            Some("drawer text".into())
        );
        assert_eq!(
            runtime.render_panel("__tab:demo-tab").expect("custom tab"),
            Some("tab text".into())
        );
        assert_eq!(
            runtime
                .render_panel("__tab:Read-only")
                .expect("readonly tab"),
            Some("one\ntwo".into())
        );
        assert_eq!(
            runtime.render_panel("__tab:Diff").expect("diff tab"),
            Some("- old\n+ new".into())
        );
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![
                PluginPanelRequest::OpenTab {
                    id: "Read-only".to_owned(),
                    title: "Read-only".to_owned(),
                    focus: true,
                },
                PluginPanelRequest::OpenTab {
                    id: "Diff".to_owned(),
                    title: "Diff".to_owned(),
                    focus: true,
                },
                PluginPanelRequest::OpenFile {
                    path: "src/main.rs".to_owned(),
                    line: Some(12),
                    readonly: true,
                },
                PluginPanelRequest::ClipboardWrite {
                    text: "plugin clipboard".to_owned(),
                },
                PluginPanelRequest::Show {
                    location: PluginPanelLocation::Sidebar,
                    focus: false,
                },
                PluginPanelRequest::Show {
                    location: PluginPanelLocation::Bottom,
                    focus: true,
                },
                PluginPanelRequest::OpenDrawer {
                    side: "left".to_owned(),
                    width: 32,
                    min_width: 12,
                },
                PluginPanelRequest::OpenTab {
                    id: "demo-tab".to_owned(),
                    title: "Demo Tab".to_owned(),
                    focus: true,
                },
                PluginPanelRequest::FocusTab {
                    id: "demo-tab".to_owned(),
                },
            ]
        );
        assert!(runtime.emit("test.event").expect("event"));
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![
                PluginPanelRequest::CloseDrawer,
                PluginPanelRequest::CloseTab {
                    id: "demo-tab".to_owned(),
                },
            ]
        );
        assert_eq!(
            runtime.logs().last().map(String::as_str),
            Some("[info] event-ran")
        );
        assert!(runtime.close_tab("demo-tab").expect("tab close callback"));
        assert_eq!(
            runtime.logs().last().map(String::as_str),
            Some("[info] tab-closed")
        );
        assert!(runtime.emit("tab.open").expect("tab.open event"));
        assert!(runtime.emit("tab.activate").expect("tab.activate event"));
        assert!(runtime.emit("tab.close").expect("tab.close event"));
        assert_eq!(
            runtime.logs().last().map(String::as_str),
            Some("[info] tab-close")
        );
        fs::remove_dir_all(root).expect("cleanup");
    }
}
