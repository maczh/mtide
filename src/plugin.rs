use std::net::IpAddr;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{error::AppResult, terminal::TerminalScreen};
use mlua::LuaSerdeExt;
use mlua::{
    Function, Lua, MetaMethod, MultiValue, RegistryKey, Table, UserData, UserDataMethods,
    Value as LuaValue, Variadic,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use ring::{
    digest::{digest, SHA256},
    rand::{SecureRandom, SystemRandom},
};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::{cell::RefCell, rc::Rc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const SUPPORTED_API: u32 = 2;
const PLUGIN_TERMINAL_OUTPUT_LIMIT: usize = 1_048_576;
const PLUGIN_STORAGE_LIMIT: usize = 1 << 20;
type PanelNodeMenus = Rc<RefCell<BTreeMap<String, Vec<(String, String)>>>>;
type PanelTitleMenus = Rc<RefCell<BTreeMap<String, Vec<(String, String)>>>>;
const PLUGIN_REGISTRY_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const PLUGIN_PACKAGE_RESPONSE_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PluginRegistryEntry {
    pub name: String,
    pub version: String,
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[allow(dead_code)]
pub fn fetch_plugin_registry(endpoint: &str) -> AppResult<Vec<PluginRegistryEntry>> {
    if !secure_plugin_endpoint(endpoint) {
        return Err(crate::error::AppError::Plugin(
            "plugin registry must use an HTTPS endpoint".into(),
        ));
    }
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| crate::error::AppError::Plugin(format!("build registry client: {error}")))?
        .get(endpoint)
        .header("Accept", "application/json")
        .header("User-Agent", "mtide")
        .send()
        .map_err(|error| {
            crate::error::AppError::Plugin(format!("fetch plugin registry: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(crate::error::AppError::Plugin(format!(
            "plugin registry returned HTTP {}",
            response.status()
        )));
    }
    let bytes = response.bytes().map_err(|error| {
        crate::error::AppError::Plugin(format!("read plugin registry: {error}"))
    })?;
    if bytes.len() > PLUGIN_REGISTRY_RESPONSE_LIMIT {
        return Err(crate::error::AppError::Plugin(
            "plugin registry response is too large".into(),
        ));
    }
    let entries: Vec<PluginRegistryEntry> = serde_json::from_slice(&bytes).map_err(|error| {
        crate::error::AppError::Plugin(format!("decode plugin registry: {error}"))
    })?;
    for entry in &entries {
        if entry.name.trim().is_empty()
            || entry.version.trim().is_empty()
            || !secure_plugin_endpoint(&entry.url)
        {
            return Err(crate::error::AppError::Plugin(
                "plugin registry contains an invalid entry".into(),
            ));
        }
    }
    Ok(entries)
}

#[allow(dead_code)]
pub fn select_plugin_registry_entry(
    entries: &[PluginRegistryEntry],
    name: &str,
    version: Option<&str>,
) -> AppResult<PluginRegistryEntry> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::error::AppError::Plugin(
            "plugin name must not be empty".into(),
        ));
    }
    let candidates: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.name == name && version.is_none_or(|version| entry.version == version)
        })
        .collect();
    let Some(entry) = candidates
        .iter()
        .max_by(|left, right| compare_plugin_versions(&left.version, &right.version))
    else {
        return Err(crate::error::AppError::Plugin(format!(
            "plugin {name} was not found in registry"
        )));
    };
    if version.is_some() && candidates.len() > 1 {
        return Err(crate::error::AppError::Plugin(format!(
            "plugin {name} has multiple matching registry entries"
        )));
    }
    Ok((*entry).clone())
}

fn compare_plugin_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let left_parts = left
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect::<Vec<_>>();
    let right_parts = right
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect::<Vec<_>>();
    left_parts.cmp(&right_parts).then_with(|| left.cmp(right))
}

#[allow(dead_code)]
pub fn download_plugin_registry_entry(entry: &PluginRegistryEntry) -> AppResult<Vec<u8>> {
    if entry.name.trim().is_empty()
        || entry.version.trim().is_empty()
        || !secure_plugin_endpoint(&entry.url)
    {
        return Err(crate::error::AppError::Plugin(
            "invalid plugin registry download entry".into(),
        ));
    }
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| crate::error::AppError::Plugin(format!("build package client: {error}")))?
        .get(&entry.url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", "mtide")
        .send()
        .map_err(|error| {
            crate::error::AppError::Plugin(format!("download plugin package: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(crate::error::AppError::Plugin(format!(
            "plugin package returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > PLUGIN_PACKAGE_RESPONSE_LIMIT as u64)
    {
        return Err(crate::error::AppError::Plugin(
            "plugin package is too large".into(),
        ));
    }
    let bytes = response
        .bytes()
        .map_err(|error| crate::error::AppError::Plugin(format!("read plugin package: {error}")))?;
    if bytes.len() > PLUGIN_PACKAGE_RESPONSE_LIMIT {
        return Err(crate::error::AppError::Plugin(
            "plugin package is too large".into(),
        ));
    }
    if let Some(expected) = &entry.sha256 {
        if !plugin_package_hash_matches(&bytes, expected) {
            return Err(crate::error::AppError::Plugin(
                "plugin package SHA-256 mismatch".into(),
            ));
        }
    }
    Ok(bytes.to_vec())
}

fn secure_plugin_endpoint(endpoint: &str) -> bool {
    if endpoint.contains([' ', '\t', '\n', '\r']) {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host_allowed = !host.is_empty()
        && !host.eq_ignore_ascii_case("localhost")
        && host
            .parse::<IpAddr>()
            .map(|address| is_public_ip(address))
            .unwrap_or(true);
    url.scheme() == "https"
        && host_allowed
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_unspecified()
                || address.is_documentation()
                || address.octets()[0] == 0
                || (address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
                || (address.octets()[0] == 198 && (18..=19).contains(&address.octets()[1])))
        }
        IpAddr::V6(address) => {
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8)
        }
    }
}

#[allow(dead_code)]
pub fn install_plugin_archive(bytes: &[u8], destination: &Path) -> AppResult<PluginPackage> {
    if bytes.is_empty() {
        return Err(crate::error::AppError::Plugin(
            "plugin package is empty".into(),
        ));
    }
    if destination.exists() {
        return Err(crate::error::AppError::Plugin(format!(
            "plugin destination already exists: {}",
            destination.display()
        )));
    }
    let parent = destination.parent().ok_or_else(|| {
        crate::error::AppError::Plugin("plugin destination has no parent directory".into())
    })?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.staging-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("plugin"),
        std::process::id()
    ));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let archive = staging.join("package.zip");
    fs::write(&archive, bytes)?;
    let listing = Command::new("unzip")
        .args(["-Z1"])
        .arg(&archive)
        .output()
        .map_err(|error| {
            crate::error::AppError::Plugin(format!("inspect plugin archive: {error}"))
        })?;
    if !listing.status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(crate::error::AppError::Plugin(
            "plugin archive is not a valid zip file".into(),
        ));
    }
    for member in String::from_utf8_lossy(&listing.stdout).lines() {
        if !safe_plugin_archive_member(member) {
            let _ = fs::remove_dir_all(&staging);
            return Err(crate::error::AppError::Plugin(
                "plugin archive contains an unsafe path".into(),
            ));
        }
    }
    let extracted = Command::new("unzip")
        .args(["-q", "-o"])
        .arg(&archive)
        .arg("-d")
        .arg(&staging)
        .status()
        .map_err(|error| {
            crate::error::AppError::Plugin(format!("extract plugin archive: {error}"))
        })?;
    if !extracted.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(crate::error::AppError::Plugin(
            "extract plugin archive failed".into(),
        ));
    }
    let _ = fs::remove_file(&archive);
    PluginPackage::load(&staging)?;
    fs::rename(&staging, destination)?;
    PluginPackage::load(destination).map_err(|error| {
        crate::error::AppError::Plugin(format!("validate installed plugin: {error}"))
    })
}

#[allow(dead_code)]
pub fn update_plugin_archive(bytes: &[u8], destination: &Path) -> AppResult<PluginPackage> {
    if !destination.is_dir() {
        return install_plugin_archive(bytes, destination);
    }
    let parent = destination.parent().ok_or_else(|| {
        crate::error::AppError::Plugin("plugin destination has no parent directory".into())
    })?;
    let backup = parent.join(format!(
        ".{}.backup-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("plugin"),
        std::process::id()
    ));
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    fs::rename(destination, &backup)?;
    match install_plugin_archive(bytes, destination) {
        Ok(package) => Ok(package),
        Err(error) => {
            if destination.exists() {
                let _ = fs::remove_dir_all(destination);
            }
            fs::rename(&backup, destination).map_err(|restore_error| {
                crate::error::AppError::Plugin(format!(
                    "plugin update failed: {error}; rollback failed: {restore_error}"
                ))
            })?;
            Err(error)
        }
    }
}

fn safe_plugin_archive_member(member: &str) -> bool {
    let member = member.trim_end_matches('/');
    !member.is_empty()
        && !member.starts_with('/')
        && !member.contains('\\')
        && member
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn plugin_package_hash_matches(bytes: &[u8], expected: &str) -> bool {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let actual = digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    expected == actual
}

fn permission_path_list(raw: &JsonValue, permission: &str) -> Vec<String> {
    raw.get("permissions")
        .and_then(|permissions| permissions.get(permission))
        .and_then(JsonValue::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn permission_is_true(raw: &JsonValue, permission: &str) -> bool {
    raw.get("permissions")
        .and_then(|permissions| permissions.get(permission))
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

fn fs_scope_roots(
    package_root: &Path,
    scopes: &[String],
    allow_all: bool,
    workspace_root: Option<&Path>,
    settings_path: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = vec![package_root.to_path_buf()];
    if allow_all {
        if let Some(workspace_root) = workspace_root {
            roots.push(workspace_root.to_path_buf());
        }
    }
    for scope in scopes {
        let expanded = if scope == "{workspace}" {
            workspace_root.map(Path::to_path_buf)
        } else if scope == "{config}" {
            settings_path.and_then(Path::parent).map(Path::to_path_buf)
        } else if let Some(relative) = scope.strip_prefix("{config}/") {
            settings_path
                .and_then(Path::parent)
                .map(|directory| directory.join(relative))
        } else if let Some(relative) = scope.strip_prefix("~/") {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
        } else if scope.starts_with('/') {
            Some(PathBuf::from(scope))
        } else if scope == "*" {
            Some(PathBuf::from(std::path::MAIN_SEPARATOR.to_string()))
        } else {
            Some(package_root.join(scope))
        };
        if let Some(expanded) = expanded {
            roots.push(expanded);
        }
    }
    roots
}

fn resolve_fs_path(package_root: &Path, config_root: &Path, value: &str) -> PathBuf {
    if let Some(relative) = value.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(relative);
        }
    }
    if value == "{config}" {
        return config_root.to_path_buf();
    }
    if let Some(relative) = value.strip_prefix("{config}/") {
        return config_root.join(relative);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        package_root.join(path)
    }
}

fn canonicalize_for_scope(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).ok();
    }
    let mut suffix = Vec::new();
    let mut current = path.to_path_buf();
    while !current.exists() {
        suffix.push(current.file_name()?.to_os_string());
        current = current.parent()?.to_path_buf();
    }
    let mut canonical = fs::canonicalize(current).ok()?;
    for component in suffix.iter().rev() {
        canonical.push(component);
    }
    Some(canonical)
}

fn fs_path_allowed(path: &Path, roots: &[PathBuf]) -> bool {
    let Some(path) = canonicalize_for_scope(path) else {
        return false;
    };
    roots
        .iter()
        .filter_map(|root| canonicalize_for_scope(root))
        .any(|root| path == root || path.starts_with(root.join("")))
}

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
    pub settings_keys: Vec<String>,
    #[serde(skip)]
    pub network_http_hosts: Vec<String>,
    #[serde(skip)]
    pub network_http_allow_all: bool,
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
    pub network_rpc_endpoints: Vec<String>,
    pub network_rpc_allow_all: bool,
    pub system_exec_commands: Vec<String>,
    pub system_exec_allow_all: bool,
    pub fs_read_scopes: Vec<String>,
    pub fs_write_scopes: Vec<String>,
    pub fs_read_allow_all: bool,
    pub fs_write_allow_all: bool,
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
    ReloadTheme,
    Show {
        location: PluginPanelLocation,
        focus: bool,
    },
    FocusTerminal,
    OpenDrawer {
        side: String,
        width: u16,
        min_width: u16,
    },
    CloseDrawer,
    OpenTab {
        id: String,
        title: String,
        file_path: Option<String>,
        focus: bool,
    },
    OpenChangesTab {
        id: String,
        title: String,
        file_path: Option<String>,
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
    FocusFileView,
    CloseFileView,
    RefreshFileView {
        id: u64,
    },
    RefreshPanel {
        id: String,
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
    Undo,
    Redo,
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
    GoToDiffHunk {
        index: usize,
    },
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
type PanelButtonActions = BTreeMap<String, Vec<(usize, RegistryKey)>>;

#[derive(Debug, Clone)]
struct PanelActionRegion {
    line: usize,
    start: usize,
    end: usize,
    command: String,
    row: usize,
}

type PanelActionRegions = BTreeMap<String, Vec<PanelActionRegion>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFileHandler {
    pub id: String,
    pub title: String,
    pub extensions: Vec<String>,
    pub magic: Option<Vec<u8>>,
}

#[derive(Debug)]
pub struct PluginRuntime {
    _lua: Lua,
    registration: Rc<RefCell<PluginRegistration>>,
    logs: Rc<RefCell<Vec<String>>>,
    notification: Rc<RefCell<Option<(String, String)>>>,
    echo_text: Rc<RefCell<String>>,
    command_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    event_callbacks: Rc<RefCell<BTreeMap<String, Vec<RegistryKey>>>>,
    panel_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    file_handler_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    file_close_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    file_event_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    panel_event_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    panel_selection_state: Rc<RefCell<BTreeMap<String, (usize, usize)>>>,
    panel_selection_dirty: Rc<RefCell<BTreeSet<String>>>,
    panel_active_selection: Rc<RefCell<BTreeMap<String, String>>>,
    next_file_close_id: Rc<RefCell<u64>>,
    last_file_close_id: RefCell<Option<u64>>,
    last_file_title: RefCell<Option<String>>,
    async_rx: Receiver<AsyncEvent>,
    async_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    async_output_callbacks: Rc<RefCell<BTreeMap<u64, AsyncOutputCallbacks>>>,
    async_cancel: Rc<RefCell<BTreeMap<u64, Arc<AtomicBool>>>>,
    net_rx: Receiver<NetCompletion>,
    net_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    spawn_rx: Receiver<SpawnEvent>,
    spawn_callbacks: Rc<RefCell<BTreeMap<u64, SpawnRegistration>>>,
    spawn_handles: Rc<RefCell<BTreeMap<u64, Arc<Mutex<SpawnHandle>>>>>,
    panel_requests: Rc<RefCell<Vec<PluginPanelRequest>>>,
    clipboard_text: Rc<RefCell<String>>,
    editor_snapshot: Rc<RefCell<PluginEditorSnapshot>>,
    diff_snapshot: Rc<RefCell<Option<PluginDiffSnapshot>>>,
    panel_size: Rc<RefCell<(u16, u16)>>,
    editor_requests: Rc<RefCell<Vec<PluginEditorRequest>>>,
    install_callback: Rc<RefCell<Option<RegistryKey>>>,
    #[allow(dead_code)]
    uninstall_callback: Rc<RefCell<Option<RegistryKey>>>,
    diagnostics: Rc<RefCell<BTreeMap<String, Vec<PluginDiagnostic>>>>,
    decorations: Rc<RefCell<BTreeMap<String, Vec<PluginDiagnostic>>>>,
    bookmarks: Rc<RefCell<PluginBookmarks>>,
    bookmark_events: Rc<RefCell<Vec<PluginBookmarkEvent>>>,
    timers: Rc<RefCell<BTreeMap<u64, PluginTimer>>>,
    automation_requests: Rc<RefCell<Vec<PluginAutomationRequest>>>,
    automation_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    automation_cancelled: Rc<RefCell<BTreeSet<u64>>>,
    dialog_callbacks: Rc<RefCell<BTreeMap<u64, RegistryKey>>>,
    command_line_callbacks: Rc<RefCell<BTreeMap<u64, PluginCommandLineCallbacks>>>,
    command_line_active: Rc<RefCell<bool>>,
    custom_dialog_callbacks: Rc<RefCell<BTreeMap<u64, PluginCustomDialogCallbacks>>>,
    dialog_widgets: Rc<RefCell<BTreeMap<u64, PluginDialogWidgetState>>>,
    drawer_actions: Rc<RefCell<Vec<(usize, RegistryKey)>>>,
    drawer_focused_button: Rc<RefCell<Option<usize>>>,
    panel_button_actions: Rc<RefCell<PanelButtonActions>>,
    panel_focused_button: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_action_regions: Rc<RefCell<PanelActionRegions>>,
    drawer_input: Rc<RefCell<Option<PluginDrawerInputState>>>,
    panel_inputs: Rc<RefCell<BTreeMap<String, PluginDrawerInputState>>>,
    panel_passwords: Rc<RefCell<BTreeMap<String, PluginDrawerInputState>>>,
    panel_checkboxes: Rc<RefCell<BTreeMap<String, PluginDrawerCheckboxState>>>,
    panel_dropdown_lines: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_focused_dropdown: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_radio_lines: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_focused_radio: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_tabs_lines: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_focused_tabs: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_scrollview_lines: Rc<RefCell<BTreeMap<String, usize>>>,
    panel_focused_scrollview: Rc<RefCell<BTreeMap<String, usize>>>,
    drawer_checkbox: Rc<RefCell<Option<PluginDrawerCheckboxState>>>,
    drawer_password: Rc<RefCell<Option<PluginDrawerInputState>>>,
    drawer_dropdown: Rc<RefCell<Option<PluginDrawerDropdownState>>>,
    rpc_rx: Receiver<RpcEvent>,
    rpc_connections: Rc<RefCell<BTreeMap<u64, RpcRegistration>>>,
    rpc_pending: Rc<RefCell<BTreeMap<(u64, u64), RegistryKey>>>,
    open_tabs: Rc<RefCell<BTreeMap<String, Option<RegistryKey>>>>,
    tab_event_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    panel_selection_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    panel_activation_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    panel_command_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    panel_key_commands: Rc<RefCell<BTreeMap<String, BTreeMap<String, String>>>>,
    tree_navigation_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    panel_node_menus: PanelNodeMenus,
    panel_title_menus: PanelTitleMenus,
    panel_title_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    status_callbacks: Rc<RefCell<BTreeMap<String, RegistryKey>>>,
    editor_context_callback: Rc<RefCell<Option<RegistryKey>>>,
    editor_context_items: Rc<RefCell<Vec<PluginContextMenuItem>>>,
    fs_read_roots: Rc<RefCell<Vec<PathBuf>>>,
    fs_write_roots: Rc<RefCell<Vec<PathBuf>>>,
    snapshot_write_roots: Rc<RefCell<Vec<PathBuf>>>,
    workspace_read_scope_enabled: bool,
    workspace_write_scope_enabled: bool,
}

#[derive(Debug)]
struct PluginTimer {
    callback: RegistryKey,
    next: Instant,
    interval: Option<Duration>,
}

#[derive(Debug)]
struct PluginContextMenuItem {
    label: String,
    callback: Option<RegistryKey>,
}

#[derive(Debug, Clone)]
struct PluginRegex(Regex);

impl UserData for PluginRegex {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("find", |lua, regex, (text, init): (String, Option<i64>)| {
            let start = regex_start_offset(text.len(), init.unwrap_or(1));
            if start > text.len() {
                return Ok(MultiValue::from_vec(vec![LuaValue::Nil]));
            }
            let mut start = start;
            while start < text.len() && !text.is_char_boundary(start) {
                start += 1;
            }
            let Some(captures) = regex.0.captures_at(&text, start) else {
                return Ok(MultiValue::from_vec(vec![LuaValue::Nil]));
            };
            let whole = captures.get(0).expect("regex capture 0 exists");
            let mut values = MultiValue::new();
            values.push_back(LuaValue::Integer((whole.start() + 1) as i64));
            values.push_back(LuaValue::Integer(whole.end() as i64));
            values.push_back(LuaValue::Table(regex_captures(lua, &captures)?));
            Ok(values)
        });
        methods.add_method(
            "find_all",
            |lua, regex, (text, limit): (String, Option<i64>)| {
                let matches = lua.create_table()?;
                let limit = limit.unwrap_or(-1);
                for (index, captures) in regex.0.captures_iter(&text).enumerate() {
                    if limit >= 0 && index >= limit as usize {
                        break;
                    }
                    let whole = captures.get(0).expect("regex capture 0 exists");
                    let item = lua.create_table()?;
                    item.set("s", whole.start() + 1)?;
                    item.set("e", whole.end())?;
                    item.set("caps", regex_captures(lua, &captures)?)?;
                    matches.set(index + 1, item)?;
                }
                Ok(matches)
            },
        );
    }
}

fn regex_start_offset(length: usize, init: i64) -> usize {
    if init < 0 {
        length.saturating_add_signed(init as isize).min(length)
    } else {
        init.saturating_sub(1) as usize
    }
}

fn regex_captures(lua: &Lua, captures: &regex::Captures<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (index, capture) in captures.iter().skip(1).enumerate() {
        match capture {
            Some(capture) => table.set(index + 1, capture.as_str())?,
            None => table.set(index + 1, false)?,
        }
    }
    Ok(table)
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
    input_cursor: usize,
    input_prefix: String,
    input_multiline: bool,
    input_wrap: bool,
    input_min_rows: usize,
    input_max_rows: usize,
    input_initialized: bool,
    input_masked: bool,
    input_on_change: Option<RegistryKey>,
    input_on_submit: Option<RegistryKey>,
    input_clear_on_submit: bool,
    input_autofocus: bool,
    buttons: Vec<PluginDialogButtonState>,
    dropdown: Option<PluginDialogDropdownState>,
    checkbox: Option<PluginDialogCheckboxState>,
    focus: PluginDialogFocus,
}

#[derive(Debug)]
struct PluginDialogCheckboxState {
    checked: bool,
    on_change: Option<RegistryKey>,
}

#[derive(Debug)]
struct PluginDialogButtonState {
    on_click: Option<RegistryKey>,
}

#[derive(Debug, Default)]
struct PluginDrawerInputState {
    line: usize,
    text: String,
    cursor: usize,
    prefix: String,
    multiline: bool,
    wrap: bool,
    min_rows: usize,
    max_rows: usize,
    initialized: bool,
    focused: bool,
    on_change: Option<RegistryKey>,
    on_submit: Option<RegistryKey>,
    clear_on_submit: bool,
    autofocus: bool,
}

/// Apply one terminal-style edit to a plugin input. The cursor is a Unicode
/// scalar index, matching the rest of the TTT compatibility layer's text
/// editing semantics. The return value distinguishes an unhandled key from a
/// handled key that did not change the value (for example Left/Right).
fn edit_plugin_input(
    text: &mut String,
    cursor: &mut usize,
    key_name: &str,
    rune: Option<char>,
    multiline: bool,
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
            *cursor = chars[..*cursor]
                .iter()
                .rposition(|character| *character == '\n')
                .map_or(0, |index| index + 1);
            Some(false)
        }
        "End" => {
            *cursor = chars[*cursor..]
                .iter()
                .position(|character| *character == '\n')
                .map_or(chars.len(), |index| *cursor + index);
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
        "Enter" if multiline => {
            chars.insert(*cursor, '\n');
            *cursor += 1;
            *text = chars.into_iter().collect();
            Some(true)
        }
        "Enter" => None,
        _ => {
            let rune = rune?;
            chars.insert(*cursor, rune);
            *cursor += 1;
            *text = chars.into_iter().collect();
            Some(true)
        }
    }
}

#[derive(Debug, Default)]
struct PluginDrawerCheckboxState {
    line: usize,
    checked: bool,
    initialized: bool,
    focused: bool,
    on_change: Option<RegistryKey>,
}

#[derive(Debug, Default)]
struct PluginDrawerDropdownState {
    line: usize,
    options: Vec<String>,
    selected: usize,
    focused: bool,
    on_change: Option<RegistryKey>,
}

#[derive(Debug)]
struct PluginDialogDropdownState {
    options: Vec<String>,
    selected: usize,
    on_change: Option<RegistryKey>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PluginDialogFocus {
    #[default]
    Input,
    Dropdown,
    Checkbox,
    Button(usize),
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
    pub viewport_top_line: usize,
    pub viewport_left_col: usize,
    pub viewport_height: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginDiffHunk {
    pub file_path: String,
    pub kind: String,
    pub line: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginDiffSnapshot {
    pub hunks: Vec<PluginDiffHunk>,
    pub current_hunk: usize,
}

#[derive(Debug)]
struct AsyncCompletion {
    id: u64,
    result: Result<(std::process::Output, bool, bool, bool), String>,
}

#[derive(Debug)]
struct AsyncOutputCallbacks {
    stdout: Option<RegistryKey>,
    stderr: Option<RegistryKey>,
}

#[derive(Debug)]
enum AsyncEvent {
    Output { id: u64, stderr: bool, data: String },
    Complete(AsyncCompletion),
}

#[derive(Clone, Debug)]
struct AsyncHandle {
    id: u64,
    cancel: Rc<RefCell<BTreeMap<u64, Arc<AtomicBool>>>>,
}

impl UserData for AsyncHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |_, handle, ()| {
            let Some(flag) = handle.cancel.borrow().get(&handle.id).cloned() else {
                return Ok(false);
            };
            Ok(!flag.swap(true, Ordering::Relaxed))
        });
        methods.add_method("id", |_, handle, ()| Ok(handle.id));
        methods.add_meta_method(MetaMethod::ToString, |_, handle, ()| {
            Ok(format!("async-handle:{}", handle.id))
        });
    }
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
    exit_code: Option<i32>,
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

fn plugin_storage_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || Path::new(name).file_name().and_then(|v| v.to_str()) != Some(name)
    {
        return Err(format!("plugin name {name:?} cannot be used for storage"));
    }
    let root = std::env::var_os("MTIDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("TTT_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|dir| dir.join(".mtide")))
        .ok_or_else(|| "plugin storage is not available".to_owned())?;
    Ok(root.join("plugin-storage").join(format!("{name}.json")))
}

fn load_plugin_storage(path: &Path) -> Result<BTreeMap<String, JsonValue>, String> {
    match fs::read(path) {
        Ok(raw) => {
            serde_json::from_slice(&raw).map_err(|error| format!("read plugin storage: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn save_plugin_storage(path: &Path, data: &BTreeMap<String, JsonValue>) -> Result<(), String> {
    if data.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
    }
    let raw = serde_json::to_vec(data).map_err(|error| error.to_string())?;
    if raw.len() > PLUGIN_STORAGE_LIMIT {
        return Err(format!(
            "plugin storage limit exceeded ({} bytes, max {})",
            raw.len(),
            PLUGIN_STORAGE_LIMIT
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| "plugin storage has no parent directory".to_owned())?;
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temp_path = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("storage"),
        std::process::id()
    ));
    fs::write(&temp_path, &raw).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.to_string());
    }
    Ok(())
}

fn settings_path(override_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    std::env::var_os("MTIDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("TTT_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|dir| dir.join(".mtide")))
        .map(|dir| dir.join("settings.json"))
        .ok_or_else(|| "settings are not available".to_owned())
}

fn load_settings(path: &Path) -> Result<JsonValue, String> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|error| format!("read settings: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_value(crate::config::Settings::default())
                .map_err(|error| format!("load default settings: {error}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn setting_allowed(key: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|pattern| {
        pattern == key
            || pattern
                .strip_suffix(".*")
                .is_some_and(|prefix| key.starts_with(&format!("{prefix}.")))
    })
}

fn setting_value<'a>(root: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    key.split('.')
        .try_fold(root, |value, segment| value.get(segment))
}

fn set_setting_value(root: &mut JsonValue, key: &str, value: JsonValue) {
    let segments: Vec<_> = key.split('.').collect();
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        return;
    }
    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        if !current.is_object() {
            *current = json!({});
        }
        let object = current.as_object_mut().expect("object created above");
        current = object
            .entry((*segment).to_owned())
            .or_insert_with(|| json!({}));
    }
    if !current.is_object() {
        *current = json!({});
    }
    current
        .as_object_mut()
        .expect("object created above")
        .insert(segments[segments.len() - 1].to_owned(), value);
}

fn remove_setting_value(root: &mut JsonValue, key: &str) {
    let segments: Vec<_> = key.split('.').collect();
    if segments.is_empty() {
        return;
    }
    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        let Some(next) = current.get_mut(*segment) else {
            return;
        };
        current = next;
    }
    if let Some(object) = current.as_object_mut() {
        object.remove(segments[segments.len() - 1]);
    }
}

fn save_settings(path: &Path, value: &JsonValue) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let dir = path
        .parent()
        .ok_or_else(|| "settings have no parent directory".to_owned())?;
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temp_path = dir.join(format!(".settings.{}.tmp", std::process::id()));
    fs::write(&temp_path, raw).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.to_string());
    }
    Ok(())
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
    allow_all: bool,
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
    let redirect_allow_all = allow_all;
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.stop();
            }
            if network_target_allowed(attempt.url().as_str(), &redirect_hosts, redirect_allow_all) {
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

fn network_target_allowed(url: &str, allowed_hosts: &[String], allow_all: bool) -> bool {
    if allow_all {
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
    pub fn load_argument(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            return Self::load(path);
        }

        let root = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut package = Self::load(root)?;
        let entry = path
            .file_name()
            .ok_or_else(|| plugin_error("plugin entry path has no file name"))?;
        let entry = entry
            .to_str()
            .ok_or_else(|| plugin_error("plugin entry path is not valid UTF-8"))?
            .to_owned();
        package.manifest.entry = Some(entry);
        package.manifest.validate(&package.root)?;
        Ok(package)
    }

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
        manifest.network_http_allow_all = permission_is_true(&raw, "network.http");
        manifest.settings_keys = raw
            .get("permissions")
            .and_then(|permissions| permissions.get("settings_keys"))
            .and_then(JsonValue::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let network_rpc_endpoints = raw
            .get("permissions")
            .and_then(|permissions| permissions.get("network.rpc"))
            .and_then(JsonValue::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let network_rpc_allow_all = raw
            .get("permissions")
            .and_then(|permissions| permissions.get("network.rpc"))
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let system_exec_commands = raw
            .get("permissions")
            .and_then(|permissions| permissions.get("system.exec"))
            .and_then(JsonValue::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let system_exec_allow_all = raw
            .get("permissions")
            .and_then(|permissions| permissions.get("system.exec"))
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let fs_read_scopes = permission_path_list(&raw, "fs.read");
        let fs_write_scopes = permission_path_list(&raw, "fs.write");
        let fs_read_allow_all = permission_is_true(&raw, "fs.read");
        let fs_write_allow_all = permission_is_true(&raw, "fs.write");
        manifest.validate(&root)?;
        Ok(Self {
            root,
            manifest,
            network_rpc_endpoints,
            network_rpc_allow_all,
            system_exec_commands,
            system_exec_allow_all,
            fs_read_scopes,
            fs_write_scopes,
            fs_read_allow_all,
            fs_write_allow_all,
        })
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
    #[allow(dead_code)]
    pub fn load(package: &PluginPackage) -> AppResult<Self> {
        Self::load_with_settings_path_and_workspace(package, None, None)
    }

    #[allow(dead_code)]
    pub fn load_with_settings_path(
        package: &PluginPackage,
        settings_override: Option<&Path>,
    ) -> AppResult<Self> {
        Self::load_with_settings_path_and_workspace(package, settings_override, None)
    }

    pub fn load_with_settings_path_and_workspace(
        package: &PluginPackage,
        settings_override: Option<&Path>,
        workspace_root: Option<&Path>,
    ) -> AppResult<Self> {
        let entry = package
            .entry_path()
            .ok_or_else(|| plugin_error("plugin has no Lua entry"))?;
        let source = fs::read_to_string(&entry)?;
        let plugin_config_root = settings_path(settings_override)
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| package.root.clone());
        let lua = Lua::new();
        configure_lua_sandbox(&lua).map_err(lua_error)?;
        let registration = Rc::new(RefCell::new(PluginRegistration::default()));
        let logs = Rc::new(RefCell::new(Vec::new()));
        let command_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let event_callbacks = Rc::new(RefCell::new(BTreeMap::<String, Vec<RegistryKey>>::new()));
        let panel_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let file_handler_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let file_close_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let file_event_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_event_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_selection_state = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_selection_dirty = Rc::new(RefCell::new(BTreeSet::new()));
        let panel_active_selection = Rc::new(RefCell::new(BTreeMap::new()));
        let next_file_close_id = Rc::new(RefCell::new(0_u64));
        let terminal_manager = Rc::new(RefCell::new(PluginTerminalManager::default()));
        let (async_tx, async_rx) = mpsc::channel();
        let async_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let async_output_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let async_cancel = Rc::new(RefCell::new(BTreeMap::new()));
        let next_async_id = Rc::new(RefCell::new(0_u64));
        let (net_tx, net_rx) = mpsc::channel();
        let net_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_net_id = Rc::new(RefCell::new(0_u64));
        let (spawn_tx, spawn_rx) = mpsc::channel();
        let spawn_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let spawn_handles = Rc::new(RefCell::new(BTreeMap::new()));
        let next_spawn_id = Rc::new(RefCell::new(0_u64));
        let panel_requests = Rc::new(RefCell::new(Vec::new()));
        let clipboard_text = Rc::new(RefCell::new(String::new()));
        let notification = Rc::new(RefCell::new(None));
        let echo_text = Rc::new(RefCell::new(String::new()));
        let editor_snapshot = Rc::new(RefCell::new(PluginEditorSnapshot::default()));
        let diff_snapshot: Rc<RefCell<Option<PluginDiffSnapshot>>> = Rc::new(RefCell::new(None));
        let panel_size = Rc::new(RefCell::new((0, 0)));
        let editor_requests = Rc::new(RefCell::new(Vec::new()));
        let install_callback = Rc::new(RefCell::new(None));
        let uninstall_callback = Rc::new(RefCell::new(None));
        let diagnostics = Rc::new(RefCell::new(BTreeMap::new()));
        let decorations = Rc::new(RefCell::new(BTreeMap::new()));
        let bookmarks = Rc::new(RefCell::new(PluginBookmarks::new()));
        let bookmark_events = Rc::new(RefCell::new(Vec::new()));
        let timers = Rc::new(RefCell::new(BTreeMap::new()));
        let next_timer_id = Rc::new(RefCell::new(0_u64));
        let automation_requests = Rc::new(RefCell::new(Vec::new()));
        let next_automation_id = Rc::new(RefCell::new(0_u64));
        let automation_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let automation_cancelled = Rc::new(RefCell::new(BTreeSet::new()));
        let dialog_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let next_dialog_id = Rc::new(RefCell::new(0_u64));
        let command_line_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let command_line_active = Rc::new(RefCell::new(false));
        let custom_dialog_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let dialog_widgets = Rc::new(RefCell::new(BTreeMap::new()));
        let drawer_actions = Rc::new(RefCell::new(Vec::new()));
        let drawer_focused_button = Rc::new(RefCell::new(None));
        let panel_button_actions = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_focused_button = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_action_regions = Rc::new(RefCell::new(BTreeMap::new()));
        let drawer_input = Rc::new(RefCell::new(None));
        let panel_inputs = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_passwords = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_checkboxes = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_dropdown_lines = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_focused_dropdown = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_radio_lines = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_focused_radio = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_tabs_lines = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_focused_tabs = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_scrollview_lines = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_focused_scrollview = Rc::new(RefCell::new(BTreeMap::new()));
        let drawer_checkbox = Rc::new(RefCell::new(None));
        let drawer_password = Rc::new(RefCell::new(None));
        let drawer_dropdown = Rc::new(RefCell::new(None));
        let next_custom_dialog_id = Rc::new(RefCell::new(0_u64));
        let (rpc_tx, rpc_rx) = mpsc::channel::<RpcEvent>();
        let rpc_connections = Rc::new(RefCell::new(BTreeMap::new()));
        let rpc_pending = Rc::new(RefCell::new(BTreeMap::new()));
        let next_rpc_id = Rc::new(RefCell::new(0_u64));
        let open_tabs = Rc::new(RefCell::new(BTreeMap::<String, Option<RegistryKey>>::new()));
        let tab_event_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let editor_context_callback = Rc::new(RefCell::new(None));
        let editor_context_items = Rc::new(RefCell::new(Vec::new()));
        let panel_selection_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_activation_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_command_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_key_commands = Rc::new(RefCell::new(BTreeMap::new()));
        let tree_navigation_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_node_menus = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_title_menus = Rc::new(RefCell::new(BTreeMap::new()));
        let panel_title_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
        let status_callbacks = Rc::new(RefCell::new(BTreeMap::new()));
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
                    let mut callback_registry = command_line_callbacks_sink.borrow_mut();
                    callback_registry.clear();
                    callback_registry.insert(id, callbacks);
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
        let notification_sink = Rc::clone(&notification);
        module
            .set(
                "notify",
                lua.create_function(move |_lua, (message, level): (String, Option<String>)| {
                    let level = level.unwrap_or_else(|| "info".to_owned());
                    notify_sink
                        .borrow_mut()
                        .push(format!("[{level}] {message}"));
                    *notification_sink.borrow_mut() = Some((message, level));
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let echo_sink = Rc::clone(&echo_text);
        module
            .set(
                "set_echo",
                lua.create_function(move |_, text: String| {
                    *echo_sink.borrow_mut() = text;
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let clear_echo_sink = Rc::clone(&echo_text);
        module
            .set(
                "clear_echo",
                lua.create_function(move |_, ()| {
                    clear_echo_sink.borrow_mut().clear();
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let status_sink = Rc::clone(&registration);
        let status_callbacks_for_set = Rc::clone(&status_callbacks);
        let status_can_write = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "set_status_item",
                lua
                    .create_function(
                        move |lua,
                              (side, id, text, options): (
                            String,
                            String,
                            String,
                            Option<Table>,
                        )| {
                            if !status_can_write {
                                return Err(mlua::Error::external("permission denied: commands"));
                            }
                            if !matches!(side.as_str(), "left" | "right") {
                                return Err(mlua::Error::external(
                                    "status item side must be left or right",
                                ));
                            }
                            let priority = options
                                .as_ref()
                                .and_then(|options| {
                                    options.get::<Option<i64>>("priority").ok().flatten()
                                })
                                .unwrap_or(1000);
                            let on_click = options.as_ref().and_then(|options| {
                                options.get::<Option<Function>>("on_click").ok().flatten()
                            });
                            let mut registration = status_sink.borrow_mut();
                            registration.status_items.retain(|item| item.id != id);
                            registration.status_items.push(PluginStatusItem {
                                id: id.clone(),
                                text,
                                side,
                                priority,
                            });
                            if let Some(previous) =
                                status_callbacks_for_set.borrow_mut().remove(&id)
                            {
                                let _ = lua.remove_registry_value(previous);
                            }
                            if let Some(callback) = on_click {
                                let key = lua.create_registry_value(callback)?;
                                status_callbacks_for_set.borrow_mut().insert(id, key);
                            }
                            Ok(())
                        },
                    )
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let status_sink = Rc::clone(&registration);
        let status_callbacks_for_remove = Rc::clone(&status_callbacks);
        let status_can_write = package
            .manifest
            .permissions
            .get("commands")
            .copied()
            .unwrap_or(false);
        module
            .set(
                "remove_status_item",
                lua.create_function(move |lua, id: String| {
                    if !status_can_write {
                        return Err(mlua::Error::external("permission denied: commands"));
                    }
                    status_sink
                        .borrow_mut()
                        .status_items
                        .retain(|item| item.id != id);
                    if let Some(callback) = status_callbacks_for_remove.borrow_mut().remove(&id) {
                        let _ = lua.remove_registry_value(callback);
                    }
                    Ok(())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let command_sink = Rc::clone(&command_callbacks);
        let command_editor_requests = Rc::clone(&editor_requests);
        let command_host_requests = Rc::clone(&panel_requests);
        let command_can_edit = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
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
                    if matches!(id.as_str(), "editor.undo" | "editor.redo") {
                        if !command_can_edit {
                            return Err(mlua::Error::external("permission denied: editor.write"));
                        }
                        command_editor_requests
                            .borrow_mut()
                            .push(if id == "editor.undo" {
                                PluginEditorRequest::Undo
                            } else {
                                PluginEditorRequest::Redo
                            });
                        return Ok(true);
                    }
                    if id == "settings.reload" {
                        command_host_requests
                            .borrow_mut()
                            .push(PluginPanelRequest::ReloadTheme);
                        return Ok(true);
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
        let tab_event_sink = Rc::clone(&tab_event_callbacks);
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
                    let on_event = config
                        .get::<Option<Function>>("on_event")?
                        .map(|callback| lua.create_registry_value(callback))
                        .transpose()?;
                    let focus = config.get::<Option<bool>>("focus")?.unwrap_or(true);
                    let panel_id = format!("__tab:{id}");
                    if let Some(previous) = tab_panel_sink.borrow_mut().remove(&panel_id) {
                        let _ = lua.remove_registry_value(previous);
                    }
                    tab_panel_sink
                        .borrow_mut()
                        .insert(panel_id, lua.create_registry_value(render)?);
                    let tab_id = id.clone();
                    if let Some(Some(previous)) =
                        open_tabs_sink.borrow_mut().insert(id.clone(), on_close)
                    {
                        let _ = lua.remove_registry_value(previous);
                    }
                    if let Some(on_event) = on_event {
                        if let Some(previous) =
                            tab_event_sink.borrow_mut().insert(id.clone(), on_event)
                        {
                            let _ = lua.remove_registry_value(previous);
                        }
                    } else if let Some(previous) = tab_event_sink.borrow_mut().remove(&id) {
                        let _ = lua.remove_registry_value(previous);
                    }
                    tab_request_sink
                        .borrow_mut()
                        .push(PluginPanelRequest::OpenTab {
                            id,
                            title,
                            file_path: None,
                            focus,
                        });
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
                    move |lua, (title, lines, file_path): (String, Table, Option<String>)| {
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
                                file_path,
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
                          (title, old_lines, new_lines, file_path): (
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
                                file_path: file_path.clone(),
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
                    let file_path = config.get::<Option<String>>("file_path")?;
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
                        .push(PluginPanelRequest::OpenChangesTab {
                            id: id.clone(),
                            title,
                            file_path,
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
        let panel_event_sink = Rc::clone(&panel_event_callbacks);
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
                                    .insert(id.clone(), lua.create_registry_value(render)?);
                            }
                            let event = sidebar.get::<Option<Function>>("on_event")?;
                            let previous = if let Some(event) = event {
                                panel_event_sink
                                    .borrow_mut()
                                    .insert(id, lua.create_registry_value(event)?)
                            } else {
                                panel_event_sink.borrow_mut().remove(&id)
                            };
                            if let Some(previous) = previous {
                                let _ = lua.remove_registry_value(previous);
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
                                    .insert(id.clone(), lua.create_registry_value(render)?);
                            }
                            let event = bottom.get::<Option<Function>>("on_event")?;
                            let previous = if let Some(event) = event {
                                panel_event_sink
                                    .borrow_mut()
                                    .insert(id, lua.create_registry_value(event)?)
                            } else {
                                panel_event_sink.borrow_mut().remove(&id)
                            };
                            if let Some(previous) = previous {
                                let _ = lua.remove_registry_value(previous);
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
        let can_file_events = package
            .manifest
            .permissions
            .get("events.file")
            .copied()
            .unwrap_or(false);
        let can_editor_events = package
            .manifest
            .permissions
            .get("events.editor")
            .copied()
            .unwrap_or(false);
        let can_bookmark_events = package
            .manifest
            .permissions
            .get("editor.bookmarks")
            .copied()
            .unwrap_or(false);
        let can_key_events = package
            .manifest
            .permissions
            .get("keybindings")
            .copied()
            .unwrap_or(false);
        events_module
            .set(
                "on",
                lua.create_function(move |lua, (event, callback): (String, Function)| {
                    let permitted = match event.as_str() {
                        "file.open" | "file.close" | "file.save" => can_file_events,
                        "editor.change" | "cursor.change" | "tab.change" => can_editor_events,
                        "bookmark.change" | "gutter.click" => can_bookmark_events,
                        "key.press" => can_key_events,
                        _ => true,
                    };
                    if !permitted {
                        return Err(mlua::Error::external(format!(
                            "permission denied for event: {event}"
                        )));
                    }
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
        let install_callback_sink = Rc::clone(&install_callback);
        module
            .set(
                "on_install",
                lua.create_function(move |lua, callback: Function| {
                    let mut hook = install_callback_sink.borrow_mut();
                    if hook.is_some() {
                        return Err(mlua::Error::external(
                            "ttt.on_install callback already registered",
                        ));
                    }
                    *hook = Some(lua.create_registry_value(callback)?);
                    Ok(true)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let uninstall_callback_sink = Rc::clone(&uninstall_callback);
        module
            .set(
                "on_uninstall",
                lua.create_function(move |lua, callback: Function| {
                    let mut hook = uninstall_callback_sink.borrow_mut();
                    if hook.is_some() {
                        return Err(mlua::Error::external(
                            "ttt.on_uninstall callback already registered",
                        ));
                    }
                    *hook = Some(lua.create_registry_value(callback)?);
                    Ok(true)
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
        let can_snapshot_write = package
            .manifest
            .permissions
            .get("fs.write")
            .copied()
            .unwrap_or(false);
        let snapshot_write_roots = Rc::new(RefCell::new(fs_scope_roots(
            &package.root,
            &package.fs_write_scopes,
            package.fs_write_allow_all,
            workspace_root,
            settings_override,
        )));
        let snapshot_root = package.root.clone();
        let snapshot_config_root = plugin_config_root.clone();
        let screenshot_write_roots = snapshot_write_roots.clone();
        module
            .set(
                "screenshot",
                lua.create_function(move |lua, path: Option<String>| {
                    if !can_screen_read {
                        return Err(mlua::Error::external("permission denied: screen.read"));
                    }
                    let text = screen_snapshot.borrow().lines.join("\n");
                    let Some(path) = path else {
                        return Ok(LuaValue::String(lua.create_string(text)?));
                    };
                    if !can_snapshot_write {
                        return Err(mlua::Error::external("permission denied: fs.write"));
                    }
                    let path = resolve_fs_path(&snapshot_root, &snapshot_config_root, &path);
                    if !fs_path_allowed(&path, &screenshot_write_roots.borrow()) {
                        return Err(mlua::Error::external(
                            "screenshot path is outside fs.write scope",
                        ));
                    }
                    fs::write(&path, text).map_err(mlua::Error::external)?;
                    Ok(LuaValue::Boolean(true))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let debug_snapshot = Rc::clone(&editor_snapshot);
        let debug_write_roots = Rc::clone(&snapshot_write_roots);
        let debug_root = package.root.clone();
        let debug_config_root = plugin_config_root.clone();
        let can_debug = can_screen_read
            && package
                .manifest
                .permissions
                .get("editor.read")
                .copied()
                .unwrap_or(false)
            && package
                .manifest
                .permissions
                .get("terminal.read")
                .copied()
                .unwrap_or(false);
        module
            .set(
                "debug",
                lua.create_function(move |lua, path: Option<String>| {
                    if !can_debug {
                        return Err(mlua::Error::external(
                            "permission denied: debug requires screen.read, editor.read, and terminal.read",
                        ));
                    }
                    let snapshot = debug_snapshot.borrow();
                    let text = serde_json::to_string(&json!({
                        "path": snapshot.path,
                        "file_name": snapshot.file_name,
                        "language": snapshot.language,
                        "cursor": {"line": snapshot.cursor_line + 1, "col": snapshot.cursor_col + 1},
                        "buffer_lines": snapshot.lines.len(),
                        "selection": snapshot.selection,
                        "cursors": snapshot.cursors,
                    }))
                    .map_err(mlua::Error::external)?;
                    let Some(path) = path else {
                        return Ok(LuaValue::String(lua.create_string(text)?));
                    };
                    if !can_snapshot_write {
                        return Err(mlua::Error::external("permission denied: fs.write"));
                    }
                    let path = resolve_fs_path(&debug_root, &debug_config_root, &path);
                    if !fs_path_allowed(&path, &debug_write_roots.borrow()) {
                        return Err(mlua::Error::external("debug path is outside fs.write scope"));
                    }
                    fs::write(&path, text).map_err(mlua::Error::external)?;
                    Ok(LuaValue::Boolean(true))
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
        let automation_cancelled_sink = Rc::clone(&automation_cancelled);
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
                        let cancel_marks = Rc::clone(&automation_cancelled_sink);
                        lua.create_function(move |_, ()| {
                            let before = cancel_sink.borrow().len();
                            cancel_sink
                                .borrow_mut()
                                .retain(|request| request.task.id != id);
                            let removed = cancel_sink.borrow().len() != before;
                            if !removed {
                                cancel_marks.borrow_mut().insert(id);
                            }
                            Ok(removed)
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
        let rpc_endpoints = package.network_rpc_endpoints.clone();
        let rpc_allow_all = package.network_rpc_allow_all;
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
                        if !rpc_endpoint_allowed(&endpoint, &rpc_endpoints, rpc_allow_all) {
                            return Err(mlua::Error::external(
                                "RPC destination is not allowed by network.rpc",
                            ));
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
                                    let callback_key = lua.create_registry_value(callback)?;
                                    request_pending.borrow_mut().insert(
                                        (connection_id, id),
                                        callback_key,
                                    );
                                    let mut stream = request_stream.lock().map_err(|_| {
                                        mlua::Error::external("RPC writer poisoned")
                                    })?;
                                    if let Err(error) = write_rpc_message(
                                        &mut stream,
                                        &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                                    ) {
                                        if let Some(callback_key) =
                                            request_pending.borrow_mut().remove(&(connection_id, id))
                                        {
                                            let _ = lua.remove_registry_value(callback_key);
                                        }
                                        return Err(mlua::Error::external(error));
                                    }
                                    let cancel_stream = Arc::clone(&request_stream);
                                    let cancel_closed = Rc::clone(&request_closed);
                                    let cancel_pending = Rc::clone(&request_pending);
                                    let cancel = lua.create_function(move |lua, ()| {
                                        if *cancel_closed.borrow() {
                                            return Ok(false);
                                        }
                                        let Some(callback_key) =
                                            cancel_pending.borrow_mut().remove(&(connection_id, id))
                                        else {
                                            return Ok(false);
                                        };
                                        let _ = lua.remove_registry_value(callback_key);
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

        let crypto_module = lua.create_table().map_err(lua_error)?;
        crypto_module
            .set(
                "base64_encode",
                lua.create_function(|_, value: String| Ok(base64_encode(value.as_bytes())))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        crypto_module
            .set(
                "base64_decode",
                lua.create_function(|lua, value: String| match base64_decode(&value) {
                    Ok(decoded) => Ok((Some(lua.create_string(&decoded)?), Option::<String>::None)),
                    Err(error) => Ok((Option::<mlua::String>::None, Some(error))),
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        crypto_module
            .set(
                "random_bytes",
                lua.create_function(|_, length: Option<usize>| {
                    let length = length.unwrap_or(16);
                    if !(1..=1024).contains(&length) {
                        return Err(mlua::Error::external(
                            "crypto.random_bytes length must be between 1 and 1024",
                        ));
                    }
                    let mut bytes = vec![0; length];
                    SystemRandom::new()
                        .fill(&mut bytes)
                        .map_err(|_| mlua::Error::external("secure random generation failed"))?;
                    Ok(hex_encode(&bytes))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        lua.globals()
            .set("crypto", crypto_module.clone())
            .map_err(lua_error)?;
        crypto_module
            .set(
                "uuid",
                lua.create_function(|_, ()| {
                    let mut bytes = [0_u8; 16];
                    SystemRandom::new()
                        .fill(&mut bytes)
                        .map_err(|_| mlua::Error::external("secure random generation failed"))?;
                    bytes[6] = (bytes[6] & 0x0f) | 0x40;
                    bytes[8] = (bytes[8] & 0x3f) | 0x80;
                    Ok(format_uuid(&bytes))
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
        let exec_commands = package.system_exec_commands.clone();
        let exec_allow_all = package.system_exec_allow_all;
        let exec_permission = can_exec;
        let exec_commands_for_sync = exec_commands.clone();
        system_module
            .set(
                "exec",
                lua.create_function(move |lua, values: Variadic<LuaValue>| {
                    if !exec_permission {
                        return Err(mlua::Error::external("permission denied: system.exec"));
                    }
                    let (argv, options) = parse_command_values(values.to_vec())?;
                    if !command_allowed(&argv, &exec_commands_for_sync, exec_allow_all) {
                        return Err(mlua::Error::external(
                            "executable is not allowed by system.exec",
                        ));
                    }
                    let (output, timed_out, truncated) = run_command(&argv, &options)?;
                    command_result(lua, output, timed_out, false, truncated)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let async_permission = can_exec;
        let exec_commands_for_async = exec_commands.clone();
        let async_sender = async_tx.clone();
        let async_callback_sink = Rc::clone(&async_callbacks);
        let async_output_callback_sink = Rc::clone(&async_output_callbacks);
        let async_cancel_sink = Rc::clone(&async_cancel);
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
                    let (argv, options, on_stdout, on_stderr) = parse_async_command_values(values)?;
                    if !command_allowed(&argv, &exec_commands_for_async, exec_allow_all) {
                        return Err(mlua::Error::external(
                            "executable is not allowed by system.exec",
                        ));
                    }
                    let id = {
                        let mut next = async_id_source.borrow_mut();
                        *next = (*next).saturating_add(1);
                        *next
                    };
                    let key = lua.create_registry_value(callback)?;
                    async_callback_sink.borrow_mut().insert(id, key);
                    async_output_callback_sink.borrow_mut().insert(
                        id,
                        AsyncOutputCallbacks {
                            stdout: on_stdout
                                .map(|callback| lua.create_registry_value(callback))
                                .transpose()?,
                            stderr: on_stderr
                                .map(|callback| lua.create_registry_value(callback))
                                .transpose()?,
                        },
                    );
                    let cancel_flag = Arc::new(AtomicBool::new(false));
                    async_cancel_sink
                        .borrow_mut()
                        .insert(id, Arc::clone(&cancel_flag));
                    let sender = async_sender.clone();
                    std::thread::spawn(move || {
                        let result = run_command_cancellable_with_output(
                            &argv,
                            &options,
                            Some(&cancel_flag),
                            Some((sender.clone(), id)),
                        )
                        .map_err(|error| error.to_string());
                        let _ = sender.send(AsyncEvent::Complete(AsyncCompletion { id, result }));
                    });
                    lua.create_userdata(AsyncHandle {
                        id,
                        cancel: Rc::clone(&async_cancel_sink),
                    })
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let async_cancel_for_lua = Rc::clone(&async_cancel);
        system_module
            .set(
                "cancel_async",
                lua.create_function(move |_, value: LuaValue| {
                    let id = match value {
                        LuaValue::Integer(id) if id >= 0 => id as u64,
                        LuaValue::Number(id) if id.is_finite() && id >= 0.0 => id as u64,
                        LuaValue::UserData(userdata) => {
                            userdata.borrow::<AsyncHandle>().map(|handle| handle.id)?
                        }
                        _ => return Ok(false),
                    };
                    let Some(flag) = async_cancel_for_lua.borrow().get(&id).cloned() else {
                        return Ok(false);
                    };
                    Ok(!flag.swap(true, Ordering::Relaxed))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let spawn_allowed = can_exec;
        let exec_commands_for_spawn = exec_commands;
        let spawn_events = spawn_tx.clone();
        let spawn_callbacks_sink = Rc::clone(&spawn_callbacks);
        let spawn_handles_sink = Rc::clone(&spawn_handles);
        let spawn_ids = Rc::clone(&next_spawn_id);
        system_module
            .set(
                "spawn",
                lua.create_function(move |lua, values: MultiValue| {
                    if !spawn_allowed {
                        return Err(mlua::Error::external("permission denied: system.exec"));
                    }
                    let mut values = values.into_iter();
                    let first = values
                        .next()
                        .ok_or_else(|| mlua::Error::external("spawn needs a command"))?;
                    let (shell, options) = match first {
                        LuaValue::String(command) => {
                            let shell = command
                                .to_str()
                                .map_err(|error| mlua::Error::external(error.to_string()))?
                                .to_owned();
                            let options = match values.next() {
                                Some(LuaValue::Table(options)) => options,
                                Some(_) => {
                                    return Err(mlua::Error::external(
                                        "spawn options must be a table",
                                    ))
                                }
                                None => lua.create_table()?,
                            };
                            if values.next().is_some() {
                                return Err(mlua::Error::external("too many spawn arguments"));
                            }
                            (shell, options)
                        }
                        LuaValue::Table(options) => {
                            if values.next().is_some() {
                                return Err(mlua::Error::external("too many spawn arguments"));
                            }
                            let shell = options
                                .get::<Option<String>>("shell")?
                                .or_else(|| options.get::<Option<String>>("command").ok().flatten())
                                .unwrap_or_else(|| "sh".to_owned());
                            (shell, options)
                        }
                        _ => {
                            return Err(mlua::Error::external(
                                "spawn command must be a string or options table",
                            ))
                        }
                    };
                    if shell.is_empty() {
                        return Err(mlua::Error::external("spawn shell must not be empty"));
                    }
                    if !command_allowed(
                        std::slice::from_ref(&shell),
                        &exec_commands_for_spawn,
                        exec_allow_all,
                    ) {
                        return Err(mlua::Error::external(
                            "executable is not allowed by system.exec",
                        ));
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
                            exit_code: None,
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
                    spawn_handles_sink
                        .borrow_mut()
                        .insert(id, Arc::clone(&handle));
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
            Rc::clone(&panel_requests),
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
        let can_editor_read = package
            .manifest
            .permissions
            .get("editor.read")
            .copied()
            .unwrap_or(false);
        let context_callback_sink = Rc::clone(&editor_context_callback);
        editor_module
            .set(
                "register_context_menu",
                lua.create_function(move |lua, callback: Function| {
                    if !can_editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let key = lua.create_registry_value(callback)?;
                    context_callback_sink.borrow_mut().replace(key);
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
                    let byte_offset = byte_offset.max(1).saturating_sub(1);
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
                    Ok(text
                        .char_indices()
                        .nth(col.max(1).saturating_sub(1))
                        .map_or(text.len() + 1, |(offset, _)| offset + 1))
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
                    let top_line = snapshot.viewport_top_line.max(1);
                    let height = snapshot
                        .viewport_height
                        .max(1)
                        .min(snapshot.lines.len().max(1));
                    viewport.set("top_line", top_line)?;
                    viewport.set(
                        "bottom_line",
                        (top_line + height - 1).min(snapshot.lines.len().max(1)),
                    )?;
                    viewport.set("height", height)?;
                    viewport.set("left_col", snapshot.viewport_left_col.max(1))?;
                    Ok(viewport)
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let diff_snapshot_source = Rc::clone(&diff_snapshot);
        editor_module
            .set(
                "diff",
                lua.create_function(move |lua, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let Some(snapshot) = diff_snapshot_source.borrow().clone() else {
                        return Ok(None);
                    };
                    let result = lua.create_table()?;
                    let hunks = lua.create_table()?;
                    for (index, hunk) in snapshot.hunks.iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("file_path", hunk.file_path.as_str())?;
                        item.set("kind", hunk.kind.as_str())?;
                        item.set("line", hunk.line + 1)?;
                        hunks.set(index + 1, item)?;
                    }
                    result.set("hunks", hunks)?;
                    result.set("current_hunk", snapshot.current_hunk)?;
                    Ok(Some(result))
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let diff_request_sink = Rc::clone(&editor_requests);
        let can_diff_write = package
            .manifest
            .permissions
            .get("editor.write")
            .copied()
            .unwrap_or(false);
        let diff_snapshot_source = Rc::clone(&diff_snapshot);
        editor_module
            .set(
                "go_to_diff_hunk",
                lua.create_function(move |_, index: usize| {
                    if !can_diff_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    let diff_state = diff_snapshot_source.borrow();
                    let Some(snapshot) = diff_state.as_ref() else {
                        return Err(mlua::Error::external("not in a diff"));
                    };
                    if index == 0 || index > snapshot.hunks.len() {
                        return Err(mlua::Error::external("diff hunk index out of range"));
                    }
                    diff_request_sink
                        .borrow_mut()
                        .push(PluginEditorRequest::GoToDiffHunk { index: index - 1 });
                    Ok(true)
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
        let editor_shadow = Rc::clone(&editor_snapshot);
        editor_module
            .set(
                "insert",
                lua.create_function(move |_, (line, col, text): (usize, usize, String)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    let request = PluginEditorRequest::Insert {
                        line: line.saturating_sub(1),
                        col: col.saturating_sub(1),
                        text,
                    };
                    editor_request_sink.borrow_mut().push(request.clone());
                    apply_editor_request_to_snapshot(&mut editor_shadow.borrow_mut(), &request);
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
        let editor_shadow = Rc::clone(&editor_snapshot);
        editor_module
            .set(
                "set_line",
                lua.create_function(move |_, (line, text): (usize, String)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    let request = PluginEditorRequest::SetLine {
                        line: line.saturating_sub(1),
                        text,
                    };
                    editor_request_sink.borrow_mut().push(request.clone());
                    apply_editor_request_to_snapshot(&mut editor_shadow.borrow_mut(), &request);
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
        let editor_shadow = Rc::clone(&editor_snapshot);
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
                        let request = PluginEditorRequest::Replace {
                            start_line: start_line.saturating_sub(1),
                            start_col: start_col.saturating_sub(1),
                            end_line: end_line.saturating_sub(1),
                            end_col: end_col.saturating_sub(1),
                            text,
                        };
                        editor_request_sink.borrow_mut().push(request.clone());
                        apply_editor_request_to_snapshot(&mut editor_shadow.borrow_mut(), &request);
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
        let editor_shadow = Rc::clone(&editor_snapshot);
        editor_module
            .set(
                "set_cursor",
                lua.create_function(move |_, (line, col): (usize, usize)| {
                    if !editor_write {
                        return Err(mlua::Error::external("permission denied: editor.write"));
                    }
                    let request = PluginEditorRequest::SetCursor {
                        line: line.saturating_sub(1),
                        col: col.saturating_sub(1),
                    };
                    editor_request_sink.borrow_mut().push(request.clone());
                    apply_editor_request_to_snapshot(&mut editor_shadow.borrow_mut(), &request);
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
                "current_word",
                lua.create_function(move |_, ()| {
                    if !editor_read {
                        return Err(mlua::Error::external("permission denied: editor.read"));
                    }
                    let snapshot = editor_snapshot_source.borrow();
                    Ok(current_word_at_cursor(
                        snapshot
                            .lines
                            .get(snapshot.cursor_line)
                            .map(String::as_str)
                            .unwrap_or_default(),
                        snapshot.cursor_col,
                    ))
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
        let decorations_sink = Rc::clone(&decorations);
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
        let decorations_sink = Rc::clone(&decorations);
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
        let fs_config_root = plugin_config_root.clone();
        let workspace_read_scope_enabled = package.fs_read_allow_all
            || package
                .fs_read_scopes
                .iter()
                .any(|scope| scope == "{workspace}");
        let workspace_write_scope_enabled = package.fs_write_allow_all
            || package
                .fs_write_scopes
                .iter()
                .any(|scope| scope == "{workspace}");
        let fs_read_roots = Rc::new(RefCell::new(fs_scope_roots(
            &package.root,
            &package.fs_read_scopes,
            package.fs_read_allow_all,
            workspace_root,
            settings_override,
        )));
        let fs_write_roots = Rc::new(RefCell::new(fs_scope_roots(
            &package.root,
            &package.fs_write_scopes,
            package.fs_write_allow_all,
            workspace_root,
            settings_override,
        )));
        let can_read = package
            .manifest
            .permissions
            .get("fs.read")
            .copied()
            .unwrap_or(false);
        let read_root = fs_root.clone();
        let read_config_root = fs_config_root.clone();
        let read_scopes = Rc::clone(&fs_read_roots);
        fs_module
            .set(
                "read",
                lua.create_function(move |lua, relative: String| {
                    if !can_read {
                        return Err(mlua::Error::external("permission denied: fs.read"));
                    }
                    let path = resolve_fs_path(&read_root, &read_config_root, &relative);
                    if !fs_path_allowed(&path, &read_scopes.borrow()) {
                        return Ok((None, Some("access denied".to_owned())));
                    }
                    match fs::read(path) {
                        Ok(contents) => {
                            Ok((Some(lua.create_string(&contents)?), Option::<String>::None))
                        }
                        Err(error) => Ok((None, Some(error.to_string()))),
                    }
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let exists_root = fs_root.clone();
        let exists_config_root = fs_config_root.clone();
        let exists_scopes = Rc::clone(&fs_read_roots);
        fs_module
            .set(
                "exists",
                lua.create_function(move |_, relative: String| {
                    if !can_read {
                        return Err(mlua::Error::external("permission denied: fs.read"));
                    }
                    let path = resolve_fs_path(&exists_root, &exists_config_root, &relative);
                    Ok(fs_path_allowed(&path, &exists_scopes.borrow()) && path.exists())
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let list_root = fs_root.clone();
        let list_config_root = fs_config_root.clone();
        let list_scopes = Rc::clone(&fs_read_roots);
        fs_module
            .set(
                "list",
                lua.create_function(move |lua, relative: Option<String>| {
                    if !can_read {
                        return Err(mlua::Error::external("permission denied: fs.read"));
                    }
                    let relative = relative.unwrap_or_default();
                    let path = resolve_fs_path(&list_root, &list_config_root, &relative);
                    if !fs_path_allowed(&path, &list_scopes.borrow()) {
                        return Ok((None, Some("access denied".to_owned())));
                    }
                    let mut entries = match fs::read_dir(path) {
                        Ok(entries) => entries,
                        Err(error) => return Ok((None, Some(error.to_string()))),
                    }
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let name = entry.file_name().into_string().ok()?;
                        let is_dir = entry.file_type().ok()?.is_dir();
                        Some((name, is_dir))
                    })
                    .collect::<Vec<_>>();
                    entries.sort_by(|left, right| left.0.cmp(&right.0));
                    let result = lua.create_table()?;
                    for (index, entry) in entries.into_iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("name", entry.0)?;
                        item.set("is_dir", entry.1)?;
                        result.set(index + 1, item)?;
                    }
                    Ok((Some(result), None))
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
        let write_scopes = Rc::clone(&fs_write_roots);
        let write_root = fs_root.clone();
        let write_config_root = fs_config_root.clone();
        fs_module
            .set(
                "write",
                lua.create_function(move |_, (relative, contents): (String, mlua::String)| {
                    if !can_write {
                        return Err(mlua::Error::external("permission denied: fs.write"));
                    }
                    let path = resolve_fs_path(&write_root, &write_config_root, &relative);
                    if !fs_path_allowed(&path, &write_scopes.borrow()) {
                        return Ok((None, Some("access denied".to_owned())));
                    }
                    match fs::write(path, contents.as_bytes()) {
                        Ok(()) => Ok((Some(true), None)),
                        Err(error) => Ok((None, Some(error.to_string()))),
                    }
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
        let network_allow_all = package.manifest.network_http_allow_all;
        let net_get = {
            let can_use = network_allowed;
            let allowed_hosts = network_hosts.clone();
            let allow_all = network_allow_all;
            lua.create_function(move |lua, (url, options): (String, Option<Table>)| {
                if !can_use {
                    return Err(mlua::Error::external("permission denied: network.http"));
                }
                if !network_target_allowed(&url, &allowed_hosts, allow_all) {
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
                let response = http_request(
                    "GET",
                    &url,
                    &headers,
                    body.as_deref(),
                    &allowed_hosts,
                    allow_all,
                );
                net_response_table(lua, response)
            })
            .map_err(lua_error)?
        };
        net_module.set("get", net_get).map_err(lua_error)?;
        let net_post = {
            let can_use = network_allowed;
            let allowed_hosts = network_hosts.clone();
            let allow_all = network_allow_all;
            lua.create_function(move |lua, (url, options): (String, Option<Table>)| {
                if !can_use {
                    return Err(mlua::Error::external("permission denied: network.http"));
                }
                if !network_target_allowed(&url, &allowed_hosts, allow_all) {
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
                let response = http_request(
                    "POST",
                    &url,
                    &headers,
                    body.as_deref(),
                    &allowed_hosts,
                    allow_all,
                );
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
        let net_get_async_allow_all = network_allow_all;
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
                    let allowed =
                        network_target_allowed(&url, &allowed_hosts, net_get_async_allow_all);
                    thread::spawn(move || {
                        let response = if allowed {
                            http_request(
                                "GET",
                                &url,
                                &headers,
                                body.as_deref(),
                                &allowed_hosts,
                                net_get_async_allow_all,
                            )
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
        let net_post_async_allow_all = network_allow_all;
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
                    let allowed =
                        network_target_allowed(&url, &allowed_hosts, net_post_async_allow_all);
                    thread::spawn(move || {
                        let response = if allowed {
                            http_request(
                                "POST",
                                &url,
                                &headers,
                                body.as_deref(),
                                &allowed_hosts,
                                net_post_async_allow_all,
                            )
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

        let settings_module = lua.create_table().map_err(lua_error)?;
        let can_settings = package
            .manifest
            .permissions
            .get("settings")
            .copied()
            .unwrap_or(false);
        if can_settings {
            let settings_keys = package.manifest.settings_keys.clone();
            let settings_path_override = settings_override.map(Path::to_path_buf);
            let get_keys = settings_keys.clone();
            let get_settings_path = settings_path_override.clone();
            settings_module
                .set(
                    "get",
                    lua.create_function(move |lua, key: String| {
                        if !setting_allowed(&key, &get_keys) {
                            return Err(mlua::Error::external(format!(
                                "permission denied: settings key {key}"
                            )));
                        }
                        let path = settings_path(get_settings_path.as_deref())
                            .map_err(mlua::Error::external)?;
                        let settings = load_settings(&path).map_err(mlua::Error::external)?;
                        setting_value(&settings, &key)
                            .map_or(Ok(LuaValue::Nil), |value| lua.to_value(value))
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
            let set_keys = settings_keys;
            let set_settings_path = settings_path_override;
            settings_module
                .set(
                    "set",
                    lua.create_function(move |lua, (key, value): (String, LuaValue)| {
                        if !setting_allowed(&key, &set_keys) {
                            return Err(mlua::Error::external(format!(
                                "permission denied: settings key {key}"
                            )));
                        }
                        let path = settings_path(set_settings_path.as_deref())
                            .map_err(mlua::Error::external)?;
                        let mut settings = load_settings(&path).map_err(mlua::Error::external)?;
                        let value: JsonValue =
                            lua.from_value(value).map_err(mlua::Error::external)?;
                        if value.is_null() {
                            remove_setting_value(&mut settings, &key);
                        } else {
                            set_setting_value(&mut settings, &key, value);
                        }
                        save_settings(&path, &settings).map_err(mlua::Error::external)?;
                        Ok(())
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }

        let regex_module = lua.create_table().map_err(lua_error)?;
        regex_module
            .set(
                "compile",
                lua.create_function(|lua, (pattern, options): (String, Option<Table>)| {
                    let ignore_case = options
                        .as_ref()
                        .and_then(|table| table.get::<Option<bool>>("ignore_case").ok().flatten())
                        .unwrap_or(false);
                    let mut builder = regex::RegexBuilder::new(&pattern);
                    builder.case_insensitive(ignore_case);
                    match builder.build() {
                        Ok(regex) => {
                            let mut result = MultiValue::new();
                            result.push_back(LuaValue::UserData(
                                lua.create_userdata(PluginRegex(regex))?,
                            ));
                            Ok(result)
                        }
                        Err(error) => {
                            let mut result = MultiValue::new();
                            result.push_back(LuaValue::Nil);
                            result
                                .push_back(LuaValue::String(lua.create_string(error.to_string())?));
                            Ok(result)
                        }
                    }
                })
                .map_err(lua_error)?,
            )
            .map_err(lua_error)?;

        let storage_module = lua.create_table().map_err(lua_error)?;
        let can_storage = package
            .manifest
            .permissions
            .get("storage")
            .copied()
            .unwrap_or(false);
        if can_storage {
            let storage_path = plugin_storage_path(&package.manifest.name)
                .map_err(mlua::Error::external)
                .map_err(lua_error)?;
            let storage = Rc::new(RefCell::new(
                load_plugin_storage(&storage_path)
                    .map_err(mlua::Error::external)
                    .map_err(lua_error)?,
            ));
            let storage_get = Rc::clone(&storage);
            storage_module
                .set(
                    "get",
                    lua.create_function(move |lua, key: String| {
                        let value = storage_get.borrow().get(&key).cloned();
                        value.map_or(Ok(LuaValue::Nil), |value| lua.to_value(&value))
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
            let storage_set = Rc::clone(&storage);
            let storage_set_path = storage_path.clone();
            storage_module
                .set(
                    "set",
                    lua.create_function(move |lua, (key, value): (String, LuaValue)| {
                        let json_value: JsonValue =
                            lua.from_value(value).map_err(mlua::Error::external)?;
                        let mut data = storage_set.borrow_mut();
                        if json_value.is_null() {
                            data.remove(&key);
                        } else {
                            data.insert(key, json_value);
                        }
                        save_plugin_storage(&storage_set_path, &data)
                            .map_err(mlua::Error::external)?;
                        Ok(())
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
            let storage_remove = Rc::clone(&storage);
            let storage_remove_path = storage_path.clone();
            storage_module
                .set(
                    "remove",
                    lua.create_function(move |_, key: String| {
                        let mut data = storage_remove.borrow_mut();
                        data.remove(&key);
                        save_plugin_storage(&storage_remove_path, &data)
                            .map_err(mlua::Error::external)?;
                        Ok(())
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
            let storage_keys = Rc::clone(&storage);
            storage_module
                .set(
                    "keys",
                    lua.create_function(move |lua, ()| {
                        let keys = lua.create_table()?;
                        for (index, key) in storage_keys.borrow().keys().enumerate() {
                            keys.set(index + 1, key.clone())?;
                        }
                        Ok(keys)
                    })
                    .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }

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
        let crypto_for_require = crypto_module.clone();
        preload
            .set(
                "ttt.crypto",
                lua.create_function(move |_, ()| Ok(crypto_for_require.clone()))
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
        let regex_for_require = regex_module.clone();
        preload
            .set(
                "ttt.regex",
                lua.create_function(move |_, ()| Ok(regex_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let settings_for_require = settings_module.clone();
        preload
            .set(
                "ttt.settings",
                lua.create_function(move |_, ()| Ok(settings_for_require.clone()))
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let storage_for_require = storage_module.clone();
        preload
            .set(
                "ttt.storage",
                lua.create_function(move |_, ()| Ok(storage_for_require.clone()))
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
            notification,
            echo_text,
            command_callbacks,
            event_callbacks,
            panel_callbacks,
            file_handler_callbacks,
            file_close_callbacks,
            file_event_callbacks,
            panel_event_callbacks,
            panel_selection_state,
            panel_selection_dirty,
            panel_active_selection,
            next_file_close_id,
            last_file_close_id: RefCell::new(None),
            last_file_title: RefCell::new(None),
            async_rx,
            async_callbacks,
            async_output_callbacks,
            async_cancel,
            net_rx,
            net_callbacks,
            spawn_rx,
            spawn_callbacks,
            spawn_handles,
            panel_requests,
            clipboard_text,
            editor_snapshot,
            diff_snapshot,
            panel_size,
            editor_requests,
            install_callback,
            uninstall_callback,
            diagnostics,
            decorations,
            bookmarks,
            bookmark_events,
            timers,
            automation_requests,
            automation_callbacks,
            automation_cancelled,
            dialog_callbacks,
            command_line_callbacks,
            command_line_active,
            custom_dialog_callbacks,
            dialog_widgets,
            drawer_actions,
            drawer_focused_button,
            panel_button_actions,
            panel_focused_button,
            panel_action_regions,
            drawer_input,
            panel_inputs,
            panel_passwords,
            panel_checkboxes,
            panel_dropdown_lines,
            panel_focused_dropdown,
            panel_radio_lines,
            panel_focused_radio,
            panel_tabs_lines,
            panel_focused_tabs,
            panel_scrollview_lines,
            panel_focused_scrollview,
            drawer_checkbox,
            drawer_password,
            drawer_dropdown,
            rpc_rx,
            rpc_connections,
            rpc_pending,
            open_tabs,
            tab_event_callbacks,
            panel_selection_callbacks,
            panel_activation_callbacks,
            panel_command_callbacks,
            panel_key_commands,
            tree_navigation_callbacks,
            panel_node_menus,
            panel_title_menus,
            panel_title_callbacks,
            status_callbacks,
            editor_context_callback,
            editor_context_items,
            fs_read_roots,
            fs_write_roots,
            snapshot_write_roots,
            workspace_read_scope_enabled,
            workspace_write_scope_enabled,
        })
    }
    pub fn add_workspace_root(&self, root: &Path) {
        let Ok(root) = root.canonicalize() else {
            return;
        };
        if self.workspace_read_scope_enabled {
            let mut roots = self.fs_read_roots.borrow_mut();
            if !roots.iter().any(|candidate| candidate == &root) {
                roots.push(root.clone());
            }
        }
        if self.workspace_write_scope_enabled {
            let mut roots = self.fs_write_roots.borrow_mut();
            if !roots.iter().any(|candidate| candidate == &root) {
                roots.push(root.clone());
            }
            let mut snapshot_roots = self.snapshot_write_roots.borrow_mut();
            if !snapshot_roots.iter().any(|candidate| candidate == &root) {
                snapshot_roots.push(root);
            }
        }
    }
    pub fn remove_workspace_root(&self, root: &Path) {
        let Ok(root) = root.canonicalize() else {
            return;
        };
        self.fs_read_roots
            .borrow_mut()
            .retain(|candidate| candidate != &root);
        self.fs_write_roots
            .borrow_mut()
            .retain(|candidate| candidate != &root);
        self.snapshot_write_roots
            .borrow_mut()
            .retain(|candidate| candidate != &root);
    }
    pub fn registration(&self) -> PluginRegistration {
        self.registration.borrow().clone()
    }
    pub fn logs(&self) -> Vec<String> {
        self.logs.borrow().clone()
    }
    pub fn echo_text(&self) -> String {
        self.echo_text.borrow().clone()
    }
    pub fn notification(&self) -> Option<(String, String)> {
        self.notification.borrow().clone()
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
        if let Some(mut state) = self.dialog_widgets.borrow_mut().remove(&id) {
            if let Some(key) = state.input_on_change.take() {
                let _ = self._lua.remove_registry_value(key);
            }
            if let Some(key) = state.input_on_submit.take() {
                let _ = self._lua.remove_registry_value(key);
            }
            for button in state.buttons.drain(..) {
                if let Some(key) = button.on_click {
                    let _ = self._lua.remove_registry_value(key);
                }
            }
            if let Some(dropdown) = state.dropdown.take() {
                if let Some(key) = dropdown.on_change {
                    let _ = self._lua.remove_registry_value(key);
                }
            }
            if let Some(checkbox) = state.checkbox.take() {
                if let Some(key) = checkbox.on_change {
                    let _ = self._lua.remove_registry_value(key);
                }
            }
            if let Some(dropdown) = state.dropdown {
                if let Some(key) = dropdown.on_change {
                    let _ = self._lua.remove_registry_value(key);
                }
            }
        }
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
    #[allow(dead_code)] // Host-side compatibility API; normal key routing uses dialog_input_key.
    pub fn dialog_input(&self, id: u64, text: String) -> AppResult<bool> {
        let callback = {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let Some(state) = widgets.get_mut(&id) else {
                return Ok(false);
            };
            state.input_text = text.clone();
            state.input_cursor = state.input_text.chars().count();
            state
                .input_on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
        };
        let Some(callback) = callback else {
            return Ok(true);
        };
        callback
            .call::<()>(text)
            .map_err(|error| plugin_error(format!("dialog input callback failed: {error}")))?;
        Ok(true)
    }
    pub fn dialog_input_key(
        &self,
        id: u64,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let (text, callback, submit, clear_on_submit) = {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let Some(state) = widgets.get_mut(&id) else {
                return Ok(false);
            };
            if state.focus != PluginDialogFocus::Input
                || (!modifier.is_empty() && key_name != "Backspace")
            {
                return Ok(false);
            }
            let submit = key_name == "Enter" && !state.input_multiline;
            if submit {
                let callback = state
                    .input_on_submit
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?;
                (
                    state.input_text.clone(),
                    callback,
                    true,
                    state.input_clear_on_submit,
                )
            } else {
                let Some(changed) = edit_plugin_input(
                    &mut state.input_text,
                    &mut state.input_cursor,
                    key_name,
                    rune,
                    state.input_multiline,
                ) else {
                    return Ok(false);
                };
                let callback = if changed {
                    state
                        .input_on_change
                        .as_ref()
                        .map(|key| self._lua.registry_value::<Function>(key))
                        .transpose()
                        .map_err(lua_error)?
                } else {
                    None
                };
                (state.input_text.clone(), callback, false, false)
            }
        };
        if let Some(callback) = callback {
            callback.call::<()>(text).map_err(|error| {
                plugin_error(if submit {
                    format!("dialog input submit failed: {error}")
                } else {
                    format!("dialog input change failed: {error}")
                })
            })?;
        }
        if submit && clear_on_submit {
            if let Some(state) = self.dialog_widgets.borrow_mut().get_mut(&id) {
                state.input_text.clear();
                state.input_cursor = 0;
            }
        }
        Ok(true)
    }
    pub fn dialog_click(&self, id: u64) -> AppResult<bool> {
        let callback = {
            let widgets = self.dialog_widgets.borrow();
            let Some(state) = widgets.get(&id) else {
                return Ok(false);
            };
            let PluginDialogFocus::Button(index) = state.focus else {
                return Ok(false);
            };
            state
                .buttons
                .get(index)
                .and_then(|button| button.on_click.as_ref())
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
        if state.dropdown.is_some() || state.checkbox.is_some() || !state.buttons.is_empty() {
            state.focus = match state.focus {
                PluginDialogFocus::Input if state.dropdown.is_some() => PluginDialogFocus::Dropdown,
                PluginDialogFocus::Input if state.checkbox.is_some() => PluginDialogFocus::Checkbox,
                PluginDialogFocus::Input => PluginDialogFocus::Button(0),
                PluginDialogFocus::Checkbox if !state.buttons.is_empty() => {
                    PluginDialogFocus::Button(0)
                }
                PluginDialogFocus::Checkbox => PluginDialogFocus::Input,
                PluginDialogFocus::Dropdown if state.checkbox.is_some() => {
                    PluginDialogFocus::Checkbox
                }
                PluginDialogFocus::Dropdown if !state.buttons.is_empty() => {
                    PluginDialogFocus::Button(0)
                }
                PluginDialogFocus::Dropdown => PluginDialogFocus::Input,
                PluginDialogFocus::Button(index) if index + 1 < state.buttons.len() => {
                    PluginDialogFocus::Button(index + 1)
                }
                PluginDialogFocus::Button(_) if state.dropdown.is_some() => {
                    PluginDialogFocus::Dropdown
                }
                PluginDialogFocus::Button(_) if state.checkbox.is_some() => {
                    PluginDialogFocus::Checkbox
                }
                PluginDialogFocus::Button(_) => PluginDialogFocus::Input,
            };
        }
        true
    }
    pub fn dialog_focus_previous(&self, id: u64) -> bool {
        let mut widgets = self.dialog_widgets.borrow_mut();
        let Some(state) = widgets.get_mut(&id) else {
            return false;
        };
        if state.dropdown.is_some() || !state.buttons.is_empty() {
            state.focus = match state.focus {
                PluginDialogFocus::Input if !state.buttons.is_empty() => {
                    PluginDialogFocus::Button(state.buttons.len() - 1)
                }
                PluginDialogFocus::Input if state.checkbox.is_some() => PluginDialogFocus::Checkbox,
                PluginDialogFocus::Input => PluginDialogFocus::Dropdown,
                PluginDialogFocus::Dropdown => PluginDialogFocus::Input,
                PluginDialogFocus::Checkbox if state.dropdown.is_some() => {
                    PluginDialogFocus::Dropdown
                }
                PluginDialogFocus::Checkbox => PluginDialogFocus::Input,
                PluginDialogFocus::Button(0) if state.checkbox.is_some() => {
                    PluginDialogFocus::Checkbox
                }
                PluginDialogFocus::Button(0) if state.dropdown.is_some() => {
                    PluginDialogFocus::Dropdown
                }
                PluginDialogFocus::Button(0) => PluginDialogFocus::Input,
                PluginDialogFocus::Button(index) => PluginDialogFocus::Button(index - 1),
            };
        }
        true
    }
    pub fn dialog_dropdown_move(&self, id: u64, delta: isize) -> AppResult<bool> {
        let callback = {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let Some(state) = widgets.get_mut(&id) else {
                return Ok(false);
            };
            if state.focus != PluginDialogFocus::Dropdown {
                return Ok(false);
            }
            let Some(dropdown) = state.dropdown.as_mut() else {
                return Ok(false);
            };
            if dropdown.options.is_empty() {
                return Ok(true);
            }
            dropdown.selected = (dropdown.selected as isize + delta)
                .rem_euclid(dropdown.options.len() as isize)
                as usize;
            dropdown
                .on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
                .map(|callback| (callback, dropdown.selected + 1))
        };
        if let Some((callback, selected)) = callback {
            callback
                .call::<()>(selected)
                .map_err(|error| plugin_error(format!("dropdown on_change failed: {error}")))?;
        }
        Ok(true)
    }
    pub fn dialog_focus_at_line(&self, id: u64, line: usize) -> bool {
        let mut widgets = self.dialog_widgets.borrow_mut();
        let Some(state) = widgets.get_mut(&id) else {
            return false;
        };
        if line == 0 {
            state.focus = PluginDialogFocus::Input;
            return true;
        }
        if state.dropdown.is_some() && line == 1 {
            state.focus = PluginDialogFocus::Dropdown;
            return true;
        }
        let checkbox_line = 1 + usize::from(state.dropdown.is_some());
        if state.checkbox.is_some() && line == checkbox_line {
            state.focus = PluginDialogFocus::Checkbox;
            return true;
        }
        let button_line =
            line.saturating_sub(checkbox_line + usize::from(state.checkbox.is_some()));
        if button_line < state.buttons.len() {
            state.focus = PluginDialogFocus::Button(button_line);
            return true;
        }
        false
    }

    pub fn dialog_focus_button(&self, id: u64) -> bool {
        let mut widgets = self.dialog_widgets.borrow_mut();
        let Some(state) = widgets.get_mut(&id) else {
            return false;
        };
        if state.buttons.is_empty() {
            return false;
        }
        state.focus = PluginDialogFocus::Button(0);
        true
    }

    pub fn dialog_activate(&self, id: u64) -> AppResult<bool> {
        let focus = self
            .dialog_widgets
            .borrow()
            .get(&id)
            .map(|state| state.focus);
        if matches!(focus, Some(PluginDialogFocus::Button(_))) {
            self.dialog_click(id)
        } else if matches!(focus, Some(PluginDialogFocus::Dropdown)) {
            self.dialog_dropdown_move(id, 0)
        } else if matches!(focus, Some(PluginDialogFocus::Checkbox)) {
            self.dialog_checkbox_toggle(id)
        } else if matches!(focus, Some(PluginDialogFocus::Input)) {
            self.dialog_submit(id)
        } else {
            Ok(false)
        }
    }

    pub fn dialog_submit(&self, id: u64) -> AppResult<bool> {
        let (callback, clear_on_submit) = {
            let widgets = self.dialog_widgets.borrow();
            let Some(state) = widgets.get(&id) else {
                return Ok(false);
            };
            if state.focus != PluginDialogFocus::Input {
                return Ok(false);
            }
            (
                state
                    .input_on_submit
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?,
                state.input_clear_on_submit,
            )
        };
        let Some(callback) = callback else {
            return Ok(false);
        };
        let text = self
            .dialog_widgets
            .borrow()
            .get(&id)
            .map(|state| state.input_text.clone())
            .unwrap_or_default();
        callback
            .call::<()>(text)
            .map_err(|error| plugin_error(format!("dialog input submit failed: {error}")))?;
        if clear_on_submit {
            if let Some(state) = self.dialog_widgets.borrow_mut().get_mut(&id) {
                state.input_text.clear();
                state.input_cursor = 0;
            }
        }
        Ok(true)
    }

    pub fn dialog_checkbox_toggle(&self, id: u64) -> AppResult<bool> {
        let callback = {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let Some(state) = widgets.get_mut(&id) else {
                return Ok(false);
            };
            if state.focus != PluginDialogFocus::Checkbox {
                return Ok(false);
            }
            let Some(checkbox) = state.checkbox.as_mut() else {
                return Ok(false);
            };
            checkbox.checked = !checkbox.checked;
            checkbox
                .on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?
        };
        if let Some(callback) = callback {
            let checked = self
                .dialog_widgets
                .borrow()
                .get(&id)
                .and_then(|s| s.checkbox.as_ref())
                .is_some_and(|c| c.checked);
            callback
                .call::<()>(checked)
                .map_err(|error| plugin_error(format!("checkbox on_change failed: {error}")))?;
        }
        Ok(true)
    }
    pub fn dialog_append(&self, id: u64, value: char) -> AppResult<bool> {
        self.dialog_input_key(id, "Char", Some(value), "")
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
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] command line on_key failed: {error}"));
                Ok(false)
            }
        }
    }
    pub fn set_clipboard_text(&self, text: &str) {
        *self.clipboard_text.borrow_mut() = text.to_owned();
    }
    pub fn set_editor_snapshot(&self, snapshot: PluginEditorSnapshot) {
        *self.editor_snapshot.borrow_mut() = snapshot;
    }
    pub fn set_diff_snapshot(&self, snapshot: Option<PluginDiffSnapshot>) {
        *self.diff_snapshot.borrow_mut() = snapshot;
    }
    pub fn set_panel_size(&self, width: u16, height: u16) {
        *self.panel_size.borrow_mut() = (width, height);
    }
    pub fn run_install_hook(&self) -> AppResult<bool> {
        let Some(callback_key) = self.install_callback.borrow_mut().take() else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&callback_key).map_err(lua_error)?;
        let result = callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("plugin install hook failed: {error}")));
        let _ = self._lua.remove_registry_value(callback_key);
        result.map(|()| true)
    }
    #[allow(dead_code)]
    pub fn run_uninstall_hook(&self) -> AppResult<bool> {
        let Some(callback_key) = self.uninstall_callback.borrow_mut().take() else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&callback_key).map_err(lua_error)?;
        let result = callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("plugin uninstall hook failed: {error}")));
        let _ = self._lua.remove_registry_value(callback_key);
        result.map(|()| true)
    }
    pub fn drain_editor_requests(&self) -> Vec<PluginEditorRequest> {
        self.editor_requests.borrow_mut().drain(..).collect()
    }
    pub fn editor_context_menu(
        &self,
        line: usize,
        col: usize,
        word: &str,
    ) -> AppResult<Vec<String>> {
        self.editor_context_items.borrow_mut().clear();
        let callback: Function = {
            let callback_keys = self.editor_context_callback.borrow();
            let Some(callback_key) = callback_keys.as_ref() else {
                return Ok(Vec::new());
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        let entries: Table = callback
            .call((line + 1, col + 1, word.to_owned()))
            .map_err(|error| plugin_error(format!("context menu callback failed: {error}")))?;
        for item in entries.sequence_values::<Table>() {
            let item = item.map_err(lua_error)?;
            if item
                .get::<Option<bool>>("separator")
                .map_err(lua_error)?
                .unwrap_or(false)
            {
                self.editor_context_items
                    .borrow_mut()
                    .push(PluginContextMenuItem {
                        label: "────────".to_owned(),
                        callback: None,
                    });
                continue;
            }
            let Some(label) = item.get::<Option<String>>("label").map_err(lua_error)? else {
                continue;
            };
            let Some(callback) = item
                .get::<Option<Function>>("on_select")
                .map_err(lua_error)?
            else {
                continue;
            };
            self.editor_context_items
                .borrow_mut()
                .push(PluginContextMenuItem {
                    label,
                    callback: Some(
                        self._lua
                            .create_registry_value(callback)
                            .map_err(lua_error)?,
                    ),
                });
        }
        Ok(self
            .editor_context_items
            .borrow()
            .iter()
            .map(|item| item.label.clone())
            .collect())
    }
    pub fn activate_editor_context_item(&self, index: usize) -> AppResult<bool> {
        let item = {
            let mut items = self.editor_context_items.borrow_mut();
            if index >= items.len() {
                return Ok(false);
            }
            items.remove(index)
        };
        let Some(callback_key) = item.callback else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&callback_key).map_err(lua_error)?;
        let result = callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("context menu item failed: {error}")));
        let _ = self._lua.remove_registry_value(callback_key);
        result.map(|()| true)
    }
    pub fn clear_editor_context_menu(&self) {
        let items = self
            .editor_context_items
            .borrow_mut()
            .drain(..)
            .collect::<Vec<_>>();
        for item in items {
            if let Some(callback) = item.callback {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
    }
    pub fn panel_context_menu(&self, panel_id: &str) -> Vec<String> {
        let items = self
            .panel_node_menus
            .borrow()
            .get(panel_id)
            .cloned()
            .or_else(|| self.panel_title_menus.borrow().get(panel_id).cloned())
            .unwrap_or_default();
        items.into_iter().map(|(label, _)| label).collect()
    }
    pub fn panel_selected_row(&self, panel_id: &str) -> usize {
        let Some(selection_key) = self.panel_active_selection.borrow().get(panel_id).cloned()
        else {
            return 1;
        };
        self.panel_selection_state
            .borrow()
            .get(&selection_key)
            .map(|(selected, _)| selected + 1)
            .unwrap_or(1)
    }
    pub fn set_panel_selected_row(&self, panel_id: &str, row: usize) -> bool {
        let Some(selection_key) = self.panel_active_selection.borrow().get(panel_id).cloned()
        else {
            return false;
        };
        let mut states = self.panel_selection_state.borrow_mut();
        let Some((selected, count)) = states.get_mut(&selection_key) else {
            return false;
        };
        if *count == 0 {
            return false;
        }
        let next = row.min(*count - 1);
        if *selected == next {
            return false;
        }
        *selected = next;
        self.panel_selection_dirty
            .borrow_mut()
            .insert(selection_key);
        true
    }
    pub fn activate_panel_context_item(
        &self,
        panel_id: &str,
        index: usize,
        row: usize,
    ) -> AppResult<bool> {
        let node_command = self
            .panel_node_menus
            .borrow()
            .get(panel_id)
            .and_then(|items| items.get(index))
            .map(|(_, command)| command.clone());
        let (command, title_menu) = if let Some(command) = node_command {
            (command, false)
        } else if let Some(command) = self
            .panel_title_menus
            .borrow()
            .get(panel_id)
            .and_then(|items| items.get(index))
            .map(|(_, command)| command.clone())
        {
            (command, true)
        } else {
            return Ok(false);
        };
        if command.is_empty() {
            return Ok(false);
        }
        if title_menu {
            let callback: Function = {
                let callbacks = self.panel_title_callbacks.borrow();
                let Some(callback_key) = callbacks.get(panel_id) else {
                    return Ok(false);
                };
                self._lua.registry_value(callback_key).map_err(lua_error)?
            };
            callback
                .call::<()>(command)
                .map_err(|error| plugin_error(format!("panel title menu failed: {error}")))?;
        } else {
            let callback: Function = {
                let callbacks = self.panel_command_callbacks.borrow();
                let Some(callback_key) = callbacks.get(panel_id) else {
                    return Ok(false);
                };
                self._lua.registry_value(callback_key).map_err(lua_error)?
            };
            callback
                .call::<()>((command, row))
                .map_err(|error| plugin_error(format!("panel context menu failed: {error}")))?;
        }
        Ok(true)
    }
    pub fn diagnostic_severity(&self, path: &str, line: usize) -> Option<u8> {
        self.diagnostics.borrow().get(path).and_then(|items| {
            items
                .iter()
                .filter(|item| {
                    !item.source.starts_with("decoration:")
                        && (item.line..=item.end_line).contains(&line)
                })
                .map(|item| item.severity)
                .min()
        })
    }
    pub fn diagnostic_message(&self, path: &str, line: usize) -> Option<String> {
        self.diagnostics.borrow().get(path).and_then(|items| {
            items
                .iter()
                .find(|item| {
                    !item.source.starts_with("decoration:")
                        && (item.line..=item.end_line).contains(&line)
                })
                .map(|item| item.message.clone())
        })
    }
    pub fn diagnostics_for_path(&self, path: &str) -> Vec<PluginDiagnostic> {
        self.diagnostics
            .borrow()
            .get(path)
            .into_iter()
            .flat_map(|items| items.iter())
            .filter(|item| !item.source.starts_with("decoration:"))
            .cloned()
            .collect()
    }
    pub fn decoration_style(&self, path: &str, line: usize) -> Option<String> {
        self.decorations.borrow().get(path).and_then(|items| {
            items
                .iter()
                .find(|item| {
                    item.source.starts_with("decoration:")
                        && (item.line..=item.end_line).contains(&line)
                })
                .and_then(|item| item.source.strip_prefix("decoration:"))
                .map(str::to_owned)
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
        self.automation_cancelled.borrow_mut().remove(&id);
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
    pub fn take_cancelled_automation(&self, id: u64) -> bool {
        self.automation_cancelled.borrow_mut().remove(&id)
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
        let callbacks = {
            let registered = self.event_callbacks.borrow();
            let Some(keys) = registered.get(event) else {
                return Ok(false);
            };
            keys.iter()
                .map(|key| self._lua.registry_value::<Function>(key).map_err(lua_error))
                .collect::<AppResult<Vec<_>>>()?
        };
        if callbacks.is_empty() {
            return Ok(false);
        }
        let protected_call: Function = self
            ._lua
            .load("return function(fn, args) return pcall(fn, table.unpack(args)) end")
            .eval()
            .map_err(lua_error)?;
        let mut first_callback_error = None;
        for callback in callbacks {
            let arguments = self._lua.create_table().map_err(lua_error)?;
            for value in payload {
                let lua_value = match value {
                    PluginEventArg::Text(value) => {
                        LuaValue::String(self._lua.create_string(value).map_err(lua_error)?)
                    }
                    PluginEventArg::Number(value) => LuaValue::Integer(*value as i64),
                    PluginEventArg::Bool(value) => LuaValue::Boolean(*value),
                };
                arguments.raw_push(lua_value).map_err(lua_error)?;
            }
            let result: MultiValue = protected_call
                .call((callback, arguments))
                .map_err(lua_error)?;
            let mut values = result.into_iter();
            let succeeded = values
                .next()
                .and_then(|value| value.as_boolean())
                .unwrap_or(false);
            if !succeeded && first_callback_error.is_none() {
                first_callback_error = values.next().map(|value| format!("{value:?}"));
            }
        }
        if let Some(error) = first_callback_error {
            return Err(plugin_error(format!("event callback failed: {error}")));
        }
        Ok(true)
    }

    pub fn emit_key_event(
        &self,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let callbacks = {
            let registered = self.event_callbacks.borrow();
            let Some(keys) = registered.get("key.press") else {
                return Ok(false);
            };
            keys.iter()
                .map(|key| self._lua.registry_value::<Function>(key).map_err(lua_error))
                .collect::<AppResult<Vec<_>>>()?
        };
        if callbacks.is_empty() {
            return Ok(false);
        }
        let event = self._lua.create_table().map_err(lua_error)?;
        event.set("type", "key").map_err(lua_error)?;
        event.set("key", key_name).map_err(lua_error)?;
        if let Some(rune) = rune {
            event.set("rune", rune.to_string()).map_err(lua_error)?;
        }
        if !modifier.is_empty() {
            event.set("mod", modifier).map_err(lua_error)?;
        }
        let mut consumed = false;
        for callback in callbacks {
            match callback.call::<bool>(event.clone()) {
                Ok(handled) => consumed |= handled,
                Err(error) => {
                    self.logs
                        .borrow_mut()
                        .push(format!("[error] key event handler failed: {error}"));
                }
            }
        }
        Ok(consumed)
    }

    pub fn poll_async(&self) -> AppResult<usize> {
        let mut completed = self.poll_timers()?;
        completed += self.poll_rpc_events()?;
        while let Ok(event) = self.async_rx.try_recv() {
            match event {
                AsyncEvent::Output { id, stderr, data } => {
                    let cancelled = self
                        .async_cancel
                        .borrow()
                        .get(&id)
                        .is_none_or(|flag| flag.load(Ordering::Relaxed));
                    if cancelled {
                        continue;
                    }
                    let callback = {
                        let callbacks = self.async_output_callbacks.borrow();
                        let key = callbacks.get(&id).and_then(|callbacks| {
                            if stderr {
                                callbacks.stderr.as_ref()
                            } else {
                                callbacks.stdout.as_ref()
                            }
                        });
                        key.map(|key| self._lua.registry_value::<Function>(key))
                            .transpose()
                            .map_err(lua_error)?
                    };
                    if let Some(callback) = callback {
                        callback.call::<()>(data).map_err(|error| {
                            plugin_error(format!("exec_async output callback failed: {error}"))
                        })?;
                        completed += 1;
                    }
                }
                AsyncEvent::Complete(completion) => {
                    self.async_cancel.borrow_mut().remove(&completion.id);
                    self.async_output_callbacks
                        .borrow_mut()
                        .remove(&completion.id);
                    let Some(key) = self.async_callbacks.borrow_mut().remove(&completion.id) else {
                        continue;
                    };
                    let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
                    let result = completion.result.map_err(plugin_error).and_then(
                        |(output, timed_out, cancelled, truncated)| {
                            command_result(&self._lua, output, timed_out, cancelled, truncated)
                                .map_err(lua_error)
                        },
                    )?;
                    callback.call::<LuaValue>(result).map_err(|error| {
                        plugin_error(format!("exec_async callback failed: {error}"))
                    })?;
                    completed += 1;
                }
            }
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
                    SpawnEventKind::Exit(_) => None,
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
            let exit_code = match &event.kind {
                SpawnEventKind::Exit(code) => *code,
                _ => None,
            };
            if let Some(callback) = callback {
                match event.kind {
                    SpawnEventKind::Stdout(text) | SpawnEventKind::Stderr(text) => {
                        callback.call::<()>(text).map_err(|error| {
                            plugin_error(format!("spawn output callback failed: {error}"))
                        })?;
                    }
                    SpawnEventKind::Exit(_) | SpawnEventKind::StreamClosed { .. } => {}
                }
            }
            if is_stream_closed {
                let should_emit_exit = {
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
                    registration.exited && !registration.stdout_open && !registration.stderr_open
                };
                if should_emit_exit {
                    let callback = {
                        let callbacks = self.spawn_callbacks.borrow();
                        callbacks
                            .get(&event.id)
                            .and_then(|registration| registration.on_exit.as_ref())
                            .map(|key| self._lua.registry_value::<Function>(key))
                            .transpose()
                            .map_err(lua_error)?
                    };
                    if let Some(callback) = callback {
                        let code = self
                            .spawn_callbacks
                            .borrow()
                            .get(&event.id)
                            .and_then(|registration| registration.exit_code)
                            .unwrap_or(-1);
                        callback
                            .call::<()>((code, LuaValue::Nil))
                            .map_err(|error| {
                                plugin_error(format!("spawn exit callback failed: {error}"))
                            })?;
                    }
                    self.spawn_callbacks.borrow_mut().remove(&event.id);
                    self.spawn_handles.borrow_mut().remove(&event.id);
                }
            } else if is_exit {
                let should_emit_exit = {
                    let mut callbacks = self.spawn_callbacks.borrow_mut();
                    let Some(registration) = callbacks.get_mut(&event.id) else {
                        continue;
                    };
                    registration.exited = true;
                    registration.exit_code = exit_code;
                    !registration.stdout_open && !registration.stderr_open
                };
                if should_emit_exit {
                    let callback = {
                        let callbacks = self.spawn_callbacks.borrow();
                        callbacks
                            .get(&event.id)
                            .and_then(|registration| registration.on_exit.as_ref())
                            .map(|key| self._lua.registry_value::<Function>(key))
                            .transpose()
                            .map_err(lua_error)?
                    };
                    if let Some(callback) = callback {
                        callback
                            .call::<()>((exit_code.unwrap_or(-1), LuaValue::Nil))
                            .map_err(|error| {
                                plugin_error(format!("spawn exit callback failed: {error}"))
                            })?;
                    }
                    self.spawn_callbacks.borrow_mut().remove(&event.id);
                    self.spawn_handles.borrow_mut().remove(&event.id);
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
                    let callback_result = callback.call::<()>(if error_value == LuaValue::Nil {
                        (result, error_value)
                    } else {
                        (LuaValue::Nil, error_value)
                    });
                    let _ = self._lua.remove_registry_value(key);
                    if let Err(error) = callback_result {
                        self.logs
                            .borrow_mut()
                            .push(format!("[error] RPC response callback failed: {error}"));
                    }
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
                    if let Err(error) = callback
                        .call::<()>((method, self._lua.to_value(&params).map_err(lua_error)?))
                    {
                        self.logs
                            .borrow_mut()
                            .push(format!("[error] RPC notification callback failed: {error}"));
                    }
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
                            let result = callback.call::<()>(());
                            let _ = self._lua.remove_registry_value(key);
                            if let Err(error) = result {
                                self.logs
                                    .borrow_mut()
                                    .push(format!("[error] RPC close callback failed: {error}"));
                            }
                        }
                    }
                    for key in pending {
                        let callback: Function =
                            self._lua.registry_value(&key).map_err(lua_error)?;
                        let result = callback
                            .call::<()>((
                                LuaValue::Nil,
                                self._lua
                                    .create_string("RPC connection closed")
                                    .map_err(lua_error)?,
                            ))
                            .map_err(lua_error);
                        let _ = self._lua.remove_registry_value(key);
                        if let Err(error) = result {
                            self.logs
                                .borrow_mut()
                                .push(format!("[error] RPC pending callback failed: {error}"));
                        }
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
        if let Some(callback) = self.panel_selection_callbacks.borrow_mut().remove(id) {
            let _ = self._lua.remove_registry_value(callback);
        }
        if let Some(callback) = self.panel_activation_callbacks.borrow_mut().remove(id) {
            let _ = self._lua.remove_registry_value(callback);
        }
        if let Some(callback) = self.panel_command_callbacks.borrow_mut().remove(id) {
            let _ = self._lua.remove_registry_value(callback);
        }
        if let Some(callback) = self.panel_title_callbacks.borrow_mut().remove(id) {
            let _ = self._lua.remove_registry_value(callback);
        }
        self.panel_title_menus.borrow_mut().remove(id);
        self.panel_key_commands.borrow_mut().remove(id);
        if let Some(actions) = self.panel_button_actions.borrow_mut().remove(id) {
            for (_, callback) in actions {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        self.panel_focused_button.borrow_mut().remove(id);
        self.panel_dropdown_lines.borrow_mut().remove(id);
        self.panel_action_regions.borrow_mut().remove(id);
        self.panel_radio_lines.borrow_mut().remove(id);
        self.panel_tabs_lines.borrow_mut().remove(id);
        self.panel_scrollview_lines.borrow_mut().remove(id);
        if id == "__drawer" {
            *self.drawer_focused_button.borrow_mut() = None;
            for (_, callback) in self.drawer_actions.borrow_mut().drain(..) {
                let _ = self._lua.remove_registry_value(callback);
            }
            let mut input = self.drawer_input.borrow_mut();
            if let Some(state) = input.as_mut() {
                if let Some(callback) = state.on_change.take() {
                    let _ = self._lua.remove_registry_value(callback);
                }
                if let Some(callback) = state.on_submit.take() {
                    let _ = self._lua.remove_registry_value(callback);
                }
            }
            if let Some(callback) = self
                .drawer_checkbox
                .borrow_mut()
                .as_mut()
                .and_then(|state| state.on_change.take())
            {
                let _ = self._lua.remove_registry_value(callback);
            }
            if let Some(callback) = self
                .drawer_dropdown
                .borrow_mut()
                .as_mut()
                .and_then(|state| state.on_change.take())
            {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        let dialog_id = id
            .strip_prefix("__dialog:")
            .and_then(|value| value.parse::<u64>().ok());
        let is_drawer = id == "__drawer";
        if let Some(dialog_id) = dialog_id {
            let mut widgets = self.dialog_widgets.borrow_mut();
            let state = widgets.entry(dialog_id).or_default();
            if let Some(key) = state.input_on_change.take() {
                let _ = self._lua.remove_registry_value(key);
            }
            if let Some(key) = state.input_on_submit.take() {
                let _ = self._lua.remove_registry_value(key);
            }
            for button in state.buttons.drain(..) {
                if let Some(key) = button.on_click {
                    let _ = self._lua.remove_registry_value(key);
                }
            }
            if let Some(dropdown) = state.dropdown.take() {
                if let Some(key) = dropdown.on_change {
                    let _ = self._lua.remove_registry_value(key);
                }
            }
        }
        let callbacks = self.panel_callbacks.borrow();
        let Some(key) = callbacks.get(id) else {
            return Ok(None);
        };
        let callback: Function = self._lua.registry_value(key).map_err(lua_error)?;
        let lines = Rc::new(RefCell::new(Vec::<String>::new()));
        let panel = self._lua.create_table().map_err(lua_error)?;
        let panel_id = id.to_owned();
        let panel_requests = Rc::clone(&self.panel_requests);
        let panel_size = Rc::clone(&self.panel_size);
        panel
            .set(
                "size",
                self._lua
                    .create_function(move |lua, _panel: Table| {
                        let (width, height) = *panel_size.borrow();
                        let result = lua.create_table()?;
                        result.set("width", width)?;
                        result.set("height", height)?;
                        Ok(result)
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        panel
            .set(
                "redraw",
                self._lua
                    .create_function(move |_, _panel: Table| {
                        panel_requests
                            .borrow_mut()
                            .push(PluginPanelRequest::RefreshPanel {
                                id: panel_id.clone(),
                            });
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        if let Some(file_id) = id
            .strip_prefix("__file:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            let panel_requests = Rc::clone(&self.panel_requests);
            panel
                .set(
                    "redraw",
                    self._lua
                        .create_function(move |_, _panel: Table| {
                            panel_requests
                                .borrow_mut()
                                .push(PluginPanelRequest::RefreshFileView { id: file_id });
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        for name in ["label", "text", "heading", "line", "status", "cell"] {
            let sink = Rc::clone(&lines);
            let is_label = name == "label";
            panel
                .set(
                    name,
                    self._lua
                        .create_function(
                            move |lua, (_panel, values): (Table, Variadic<LuaValue>)| {
                                let values = values.into_iter().collect::<Vec<_>>();
                                if is_label && values.len() == 1 {
                                    if let Some(LuaValue::Table(value)) = values.first() {
                                        sink.borrow_mut().push(label_widget_text(
                                            lua,
                                            &LuaValue::Table(value.clone()),
                                        )?);
                                        return Ok(());
                                    }
                                }
                                if values.len() >= 3 {
                                    let x = lua_value_usize(values.first());
                                    let y = lua_value_usize(values.get(1));
                                    if let (Some(x), Some(y), Some(value)) =
                                        (x, y, values.get(2).cloned())
                                    {
                                        raw_panel_text(
                                            &mut sink.borrow_mut(),
                                            x,
                                            y,
                                            &lua_value_text(lua, value),
                                        );
                                        return Ok(());
                                    }
                                }
                                if let Some(value) = values.first().cloned() {
                                    sink.borrow_mut().push(lua_value_text(lua, value));
                                }
                                Ok(())
                            },
                        )
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let markdown_sink = Rc::clone(&lines);
        panel
            .set(
                "markdown",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(value) = value {
                            markdown_sink
                                .borrow_mut()
                                .extend(markdown_widget_lines(lua, value)?);
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let tree_sink = Rc::clone(&lines);
        let tree_panel_id = id.to_owned();
        let tree_selection_callbacks = Rc::clone(&self.panel_selection_callbacks);
        let tree_selection_state = Rc::clone(&self.panel_selection_state);
        let tree_selection_dirty = Rc::clone(&self.panel_selection_dirty);
        let tree_active_selection = Rc::clone(&self.panel_active_selection);
        let tree_command_callbacks = Rc::clone(&self.panel_command_callbacks);
        let tree_key_commands = Rc::clone(&self.panel_key_commands);
        let tree_navigation_callbacks = Rc::clone(&self.tree_navigation_callbacks);
        let tree_node_menus = Rc::clone(&self.panel_node_menus);
        let tree_activation_callbacks = Rc::clone(&self.panel_activation_callbacks);
        let tree_panel_size = Rc::clone(&self.panel_size);
        panel
            .set(
                "tree",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            let selection_key = format!("{tree_panel_id}:tree");
                            tree_active_selection
                                .borrow_mut()
                                .insert(tree_panel_id.clone(), selection_key.clone());
                            let visible_count = tree_visible_nodes(&value).len();
                            if visible_count > 0 {
                                let configured = value.get::<Option<u64>>("selected")?;
                                let configured_index =
                                    configured.unwrap_or(1).saturating_sub(1) as usize;
                                let selected_index = {
                                    let mut states = tree_selection_state.borrow_mut();
                                    let selected = states.entry(selection_key.clone()).or_insert((
                                        configured_index.min(visible_count - 1),
                                        visible_count,
                                    ));
                                    if !tree_selection_dirty.borrow().contains(&selection_key) {
                                        selected.0 = configured_index.min(visible_count - 1);
                                    }
                                    selected.1 = visible_count;
                                    selected.0 = selected.0.min(visible_count - 1);
                                    selected.0
                                };
                                if configured.is_some()
                                    || tree_selection_dirty.borrow().contains(&selection_key)
                                {
                                    value.set("selected", (selected_index + 1) as u64)?;
                                }
                            }
                            let on_select = value.get::<Option<Function>>("on_select")?;
                            let on_expand = value.get::<Option<Function>>("on_expand")?;
                            if let Some(callback) = value.get::<Option<Function>>("on_command")? {
                                let tree_value = value.clone();
                                let callback = lua.create_function(
                                    move |_, (command, row): (String, usize)| {
                                        let item = tree_visible_nodes(&tree_value)
                                            .get(row.saturating_sub(1))
                                            .cloned()
                                            .unwrap_or(LuaValue::Nil);
                                        callback.call::<()>((command, item))
                                    },
                                )?;
                                let key = lua.create_registry_value(callback)?;
                                if let Some(previous) = tree_command_callbacks
                                    .borrow_mut()
                                    .insert(tree_panel_id.clone(), key)
                                {
                                    let _ = lua.remove_registry_value(previous);
                                }
                            }
                            if let Some(callback) = value.get::<Option<Function>>("on_command")? {
                                let tree_value = value.clone();
                                let callback = lua.create_function(move |_, row: usize| {
                                    let item = tree_visible_nodes(&tree_value)
                                        .get(row.saturating_sub(1))
                                        .cloned()
                                        .unwrap_or(LuaValue::Nil);
                                    let is_leaf = match &item {
                                        LuaValue::Table(item) => {
                                            item.get::<Option<Table>>("children")?.is_none()
                                        }
                                        _ => true,
                                    };
                                    if is_leaf {
                                        callback.call::<()>(("activate", item))?;
                                    }
                                    Ok(())
                                })?;
                                let key = lua.create_registry_value(callback)?;
                                if let Some(previous) = tree_activation_callbacks
                                    .borrow_mut()
                                    .insert(tree_panel_id.clone(), key)
                                {
                                    let _ = lua.remove_registry_value(previous);
                                }
                            }
                            let mut bindings = BTreeMap::new();
                            if let Some(commands) = value.get::<Option<Table>>("key_commands")? {
                                for pair in commands.pairs::<String, String>() {
                                    let (key, command) = pair?;
                                    bindings.insert(key, command);
                                }
                            }
                            tree_key_commands
                                .borrow_mut()
                                .insert(tree_panel_id.clone(), bindings);
                            let node_menu = panel_node_menu_entries(lua, &value)?;
                            if node_menu.is_empty() {
                                tree_node_menus.borrow_mut().remove(&tree_panel_id);
                            } else {
                                tree_node_menus
                                    .borrow_mut()
                                    .insert(tree_panel_id.clone(), node_menu);
                            }
                            if on_select.is_some() || on_expand.is_some() {
                                let tree_value = value.clone();
                                let selection_callback = on_select.clone();
                                let expand_callback = on_expand.clone();
                                let callback = lua.create_function(move |_, row: usize| {
                                    let item = tree_visible_nodes(&tree_value)
                                        .get(row.saturating_sub(1))
                                        .cloned()
                                        .unwrap_or(LuaValue::Nil);
                                    let item_table = match item {
                                        LuaValue::Table(table) => Some(table),
                                        _ => None,
                                    };
                                    if let Some(item_table) = item_table {
                                        let has_children =
                                            item_table.get::<Option<Table>>("children")?.is_some();
                                        if has_children
                                            && !item_table
                                                .get::<Option<bool>>("expanded")?
                                                .unwrap_or(false)
                                        {
                                            item_table.set("expanded", true)?;
                                            if let Some(callback) = expand_callback.as_ref() {
                                                callback.call::<()>(item_table.clone())?;
                                            }
                                        }
                                        if let Some(callback) = selection_callback.as_ref() {
                                            callback.call::<()>(item_table)?;
                                        }
                                    } else if let Some(callback) = selection_callback.as_ref() {
                                        callback.call::<()>(LuaValue::Nil)?;
                                    }
                                    Ok(())
                                })?;
                                let key = lua.create_registry_value(callback)?;
                                if let Some(previous) = tree_selection_callbacks
                                    .borrow_mut()
                                    .insert(tree_panel_id.clone(), key)
                                {
                                    let _ = lua.remove_registry_value(previous);
                                }
                            }
                            if on_expand.is_some() {
                                let tree_value = value.clone();
                                let expand_callback = on_expand.clone();
                                let callback =
                                    lua.create_function(move |_, (row, expand): (usize, bool)| {
                                        let Some(LuaValue::Table(item)) =
                                            tree_visible_nodes(&tree_value)
                                                .get(row.saturating_sub(1))
                                                .cloned()
                                        else {
                                            return Ok(false);
                                        };
                                        if item.get::<Option<Table>>("children")?.is_none() {
                                            return Ok(false);
                                        }
                                        let was_expanded =
                                            item.get::<Option<bool>>("expanded")?.unwrap_or(false);
                                        item.set("expanded", expand)?;
                                        if expand && !was_expanded {
                                            if let Some(callback) = expand_callback.as_ref() {
                                                callback.call::<()>(item)?;
                                            }
                                        }
                                        Ok(was_expanded != expand)
                                    })?;
                                let key = lua.create_registry_value(callback)?;
                                if let Some(previous) = tree_navigation_callbacks
                                    .borrow_mut()
                                    .insert(tree_panel_id.clone(), key)
                                {
                                    let _ = lua.remove_registry_value(previous);
                                }
                            }
                            let width = usize::from(tree_panel_size.borrow().0.saturating_sub(2));
                            tree_sink.borrow_mut().push(tree_widget_text_with_width(
                                lua,
                                &value,
                                Some(width),
                            ));
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let divider_sink = Rc::clone(&lines);
        panel
            .set(
                "divider",
                self._lua
                    .create_function(move |_, _panel: Table| {
                        divider_sink.borrow_mut().push("────────".to_owned());
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let clear_sink = Rc::clone(&lines);
        panel
            .set(
                "clear",
                self._lua
                    .create_function(
                        move |_, (_panel, x, y, width, height): (Table, i64, i64, i64, i64)| {
                            clear_panel_region(
                                &mut clear_sink.borrow_mut(),
                                x.max(0) as usize,
                                y.max(0) as usize,
                                width.max(0) as usize,
                                height.max(0) as usize,
                            );
                            Ok(())
                        },
                    )
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let keyvalue_sink = Rc::clone(&lines);
        panel
            .set(
                "keyvalue",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<Table>)| {
                        let Some(value) = value else {
                            return Ok(());
                        };
                        for item in value.sequence_values::<LuaValue>() {
                            let item = item?;
                            if let LuaValue::Table(item) = item {
                                let key = item.get::<Option<String>>("key")?.unwrap_or_default();
                                let value = item
                                    .get::<Option<LuaValue>>("value")?
                                    .map(|value| lua_value_text(lua, value))
                                    .unwrap_or_default();
                                keyvalue_sink.borrow_mut().push(format!("{key}: {value}"));
                            }
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let input_sink = Rc::clone(&lines);
        let input_panel_size = Rc::clone(&self.panel_size);
        let input_widgets = Rc::clone(&self.dialog_widgets);
        let drawer_input = Rc::clone(&self.drawer_input);
        let panel_inputs = Rc::clone(&self.panel_inputs);
        let panel_input_id = id.to_owned();
        panel
            .set(
                "input",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        let Some(LuaValue::Table(value)) = value else {
                            return Ok(());
                        };
                        let controlled_text = value
                            .get::<Option<LuaValue>>("value")?
                            .filter(|value| !matches!(value, LuaValue::Nil))
                            .map(|value| lua_value_text(lua, value));
                        let text = controlled_text
                            .clone()
                            .or(value.get::<Option<String>>("text")?)
                            .unwrap_or_default();
                        let prefix = value.get::<Option<String>>("prefix")?.unwrap_or_default();
                        if let Some(dialog_id) = dialog_id {
                            let mut widgets = input_widgets.borrow_mut();
                            let state = widgets.entry(dialog_id).or_default();
                            state.input_prefix = prefix.clone();
                            state.input_multiline =
                                value.get::<Option<bool>>("multiline")?.unwrap_or(false);
                            state.input_wrap = value.get::<Option<bool>>("wrap")?.unwrap_or(false);
                            state.input_min_rows = value
                                .get::<Option<u64>>("min_rows")?
                                .unwrap_or(1)
                                .clamp(1, 64)
                                as usize;
                            state.input_max_rows = value
                                .get::<Option<u64>>("max_rows")?
                                .unwrap_or(if state.input_multiline { 5 } else { 1 })
                                .clamp(1, 64)
                                as usize;
                            state.input_min_rows = state.input_min_rows.min(state.input_max_rows);
                            if controlled_text.is_some() {
                                state.input_text = text.clone();
                                state.input_cursor = state.input_text.chars().count();
                            } else if !state.input_initialized {
                                state.input_text = text.clone();
                                state.input_cursor = state.input_text.chars().count();
                                state.input_initialized = true;
                            }
                            state.input_clear_on_submit = value
                                .get::<Option<bool>>("clear_on_submit")?
                                .unwrap_or(false);
                            state.input_autofocus =
                                value.get::<Option<bool>>("autofocus")?.unwrap_or(false);
                            if let Some(callback) = value.get::<Option<Function>>("on_change")? {
                                if let Some(existing) = state.input_on_change.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                state.input_on_change = Some(lua.create_registry_value(callback)?);
                            }
                            if let Some(callback) = value.get::<Option<Function>>("on_submit")? {
                                if let Some(existing) = state.input_on_submit.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                state.input_on_submit = Some(lua.create_registry_value(callback)?);
                            }
                        } else if is_drawer {
                            let mut state = drawer_input.borrow_mut();
                            let state = state.get_or_insert_with(PluginDrawerInputState::default);
                            state.prefix = prefix.clone();
                            state.multiline =
                                value.get::<Option<bool>>("multiline")?.unwrap_or(false);
                            state.wrap = value.get::<Option<bool>>("wrap")?.unwrap_or(false);
                            state.min_rows = value
                                .get::<Option<u64>>("min_rows")?
                                .unwrap_or(1)
                                .clamp(1, 64) as usize;
                            state.max_rows = value
                                .get::<Option<u64>>("max_rows")?
                                .unwrap_or(if state.multiline { 5 } else { 1 })
                                .clamp(1, 64) as usize;
                            state.min_rows = state.min_rows.min(state.max_rows);
                            if controlled_text.is_some() {
                                state.text = text.clone();
                                state.cursor = state.text.chars().count();
                            } else if !state.initialized {
                                state.text = text.clone();
                                state.cursor = state.text.chars().count();
                                state.initialized = true;
                            }
                            state.cursor = state.cursor.min(state.text.chars().count());
                            state.clear_on_submit = value
                                .get::<Option<bool>>("clear_on_submit")?
                                .unwrap_or(false);
                            state.autofocus =
                                value.get::<Option<bool>>("autofocus")?.unwrap_or(false);
                            state.line = input_sink.borrow().len();
                            if let Some(callback) = value.get::<Option<Function>>("on_change")? {
                                if let Some(existing) = state.on_change.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                state.on_change = Some(lua.create_registry_value(callback)?);
                            }
                            if let Some(callback) = value.get::<Option<Function>>("on_submit")? {
                                if let Some(existing) = state.on_submit.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                state.on_submit = Some(lua.create_registry_value(callback)?);
                            }
                        } else {
                            let mut states = panel_inputs.borrow_mut();
                            let state = states.entry(panel_input_id.clone()).or_default();
                            state.prefix = prefix.clone();
                            state.multiline =
                                value.get::<Option<bool>>("multiline")?.unwrap_or(false);
                            state.wrap = value.get::<Option<bool>>("wrap")?.unwrap_or(false);
                            state.min_rows = value
                                .get::<Option<u64>>("min_rows")?
                                .unwrap_or(1)
                                .clamp(1, 64) as usize;
                            state.max_rows = value
                                .get::<Option<u64>>("max_rows")?
                                .unwrap_or(if state.multiline { 5 } else { 1 })
                                .clamp(1, 64) as usize;
                            state.min_rows = state.min_rows.min(state.max_rows);
                            if controlled_text.is_some() {
                                state.text = text.clone();
                                state.cursor = state.text.chars().count();
                            } else if !state.initialized {
                                state.text = text.clone();
                                state.cursor = state.text.chars().count();
                                state.initialized = true;
                            }
                            state.cursor = state.cursor.min(state.text.chars().count());
                            state.clear_on_submit = value
                                .get::<Option<bool>>("clear_on_submit")?
                                .unwrap_or(false);
                            state.autofocus =
                                value.get::<Option<bool>>("autofocus")?.unwrap_or(false);
                            state.line = input_sink.borrow().len();
                            if let Some(existing) = state.on_change.take() {
                                let _ = lua.remove_registry_value(existing);
                            }
                            if let Some(callback) = value.get::<Option<Function>>("on_change")? {
                                state.on_change = Some(lua.create_registry_value(callback)?);
                            }
                            if let Some(existing) = state.on_submit.take() {
                                let _ = lua.remove_registry_value(existing);
                            }
                            if let Some(callback) = value.get::<Option<Function>>("on_submit")? {
                                state.on_submit = Some(lua.create_registry_value(callback)?);
                            }
                        }
                        let placeholder = value
                            .get::<Option<String>>("placeholder")?
                            .unwrap_or_default();
                        let displayed = if let Some(dialog_id) = dialog_id {
                            input_widgets
                                .borrow()
                                .get(&dialog_id)
                                .map(|state| {
                                    if state.input_masked {
                                        "•".repeat(state.input_text.chars().count())
                                    } else {
                                        state.input_text.clone()
                                    }
                                })
                                .unwrap_or(text)
                        } else if is_drawer {
                            drawer_input
                                .borrow()
                                .as_ref()
                                .map(|state| state.text.clone())
                                .unwrap_or(text)
                        } else {
                            panel_inputs
                                .borrow()
                                .get(&panel_input_id)
                                .map(|state| state.text.clone())
                                .unwrap_or(text)
                        };
                        let (prefix, input_focused) = if let Some(dialog_id) = dialog_id {
                            let state = input_widgets.borrow();
                            let prefix = state
                                .get(&dialog_id)
                                .map(|state| state.input_prefix.clone())
                                .unwrap_or_default();
                            let focused = state
                                .get(&dialog_id)
                                .is_some_and(|state| state.focus == PluginDialogFocus::Input);
                            (prefix, focused)
                        } else if is_drawer {
                            let state = drawer_input.borrow();
                            let prefix = state
                                .as_ref()
                                .map(|state| state.prefix.clone())
                                .unwrap_or_default();
                            let focused = state.as_ref().is_some_and(|state| state.focused);
                            (prefix, focused)
                        } else {
                            let prefix = panel_inputs
                                .borrow()
                                .get(&panel_input_id)
                                .map(|state| state.prefix.clone())
                                .unwrap_or_default();
                            (prefix, false)
                        };
                        let focused = is_drawer && dialog_id.is_none() && input_focused;
                        let mut rendered = if displayed.is_empty() && !placeholder.is_empty() {
                            format!("{prefix}[{placeholder}]")
                        } else {
                            format!("{prefix}{}❯ {displayed}", if focused { "▶" } else { "" })
                        };
                        let min_rows = if let Some(dialog_id) = dialog_id {
                            1.max(
                                input_widgets
                                    .borrow()
                                    .get(&dialog_id)
                                    .map(|state| state.input_min_rows)
                                    .unwrap_or(1),
                            )
                        } else if is_drawer {
                            drawer_input
                                .borrow()
                                .as_ref()
                                .map(|state| state.min_rows)
                                .unwrap_or(1)
                        } else {
                            panel_inputs
                                .borrow()
                                .get(&panel_input_id)
                                .map(|state| state.min_rows)
                                .unwrap_or(1)
                        };
                        let max_rows = if let Some(dialog_id) = dialog_id {
                            input_widgets
                                .borrow()
                                .get(&dialog_id)
                                .map(|state| state.input_max_rows)
                                .unwrap_or(1)
                        } else if is_drawer {
                            drawer_input
                                .borrow()
                                .as_ref()
                                .map(|state| state.max_rows)
                                .unwrap_or(1)
                        } else {
                            panel_inputs
                                .borrow()
                                .get(&panel_input_id)
                                .map(|state| state.max_rows)
                                .unwrap_or(1)
                        };
                        let wrap = if let Some(dialog_id) = dialog_id {
                            input_widgets
                                .borrow()
                                .get(&dialog_id)
                                .is_some_and(|state| state.input_wrap)
                        } else if is_drawer {
                            drawer_input
                                .borrow()
                                .as_ref()
                                .is_some_and(|state| state.wrap)
                        } else {
                            panel_inputs
                                .borrow()
                                .get(&panel_input_id)
                                .is_some_and(|state| state.wrap)
                        };
                        if wrap {
                            let width = usize::from(input_panel_size.borrow().0).saturating_sub(2);
                            if width > 0 {
                                rendered = wrap_plain_text(&rendered, width);
                            }
                        }
                        let rendered_rows = rendered.split('\n').count();
                        if rendered_rows > max_rows {
                            rendered = rendered
                                .split('\n')
                                .take(max_rows)
                                .collect::<Vec<_>>()
                                .join("\n");
                        }
                        let rendered_rows = rendered.split('\n').count();
                        if rendered_rows < min_rows {
                            rendered.push_str(&"\n".repeat(min_rows - rendered_rows));
                        }
                        input_sink.borrow_mut().push(rendered);
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let button_sink = Rc::clone(&lines);
        let button_widgets = Rc::clone(&self.dialog_widgets);
        let drawer_actions = Rc::clone(&self.drawer_actions);
        let drawer_focused_button = Rc::clone(&self.drawer_focused_button);
        let panel_button_actions = Rc::clone(&self.panel_button_actions);
        let button_panel_id = id.to_owned();
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
                        let disabled = value.get::<Option<bool>>("disabled")?.unwrap_or(false);
                        if let Some(dialog_id) = dialog_id {
                            let mut widgets = button_widgets.borrow_mut();
                            let state = widgets.entry(dialog_id).or_default();
                            let on_click = (!disabled)
                                .then(|| value.get::<Option<Function>>("on_click"))
                                .transpose()?
                                .flatten()
                                .map(|callback| lua.create_registry_value(callback))
                                .transpose()?;
                            state.buttons.push(PluginDialogButtonState { on_click });
                        } else if is_drawer {
                            if !disabled {
                                if let Some(callback) = value.get::<Option<Function>>("on_click")? {
                                let key = lua.create_registry_value(callback)?;
                                let line = button_sink.borrow().len();
                                let mut actions = drawer_actions.borrow_mut();
                                let index = actions.len();
                                actions.push((line, key));
                                let focused = drawer_focused_button
                                    .borrow()
                                    .is_some_and(|focused| focused == index);
                                button_sink.borrow_mut().push(if focused {
                                    format!("▶[ {label} ]")
                                } else {
                                    format!("[ {label} ]")
                                });
                                return Ok(());
                                }
                            }
                        } else if !disabled {
                            if let Some(callback) = value.get::<Option<Function>>("on_click")? {
                            let key = lua.create_registry_value(callback)?;
                            let line = button_sink.borrow().len();
                            panel_button_actions
                                .borrow_mut()
                                .entry(button_panel_id.clone())
                                .or_default()
                                .push((line, key));
                            }
                        }
                        let focused = dialog_id.is_some_and(|dialog_id| {
                            let widgets = button_widgets.borrow();
                            let Some(state) = widgets.get(&dialog_id) else {
                                return false;
                            };
                            matches!(state.focus, PluginDialogFocus::Button(index) if index + 1 == state.buttons.len())
                        });
                        button_sink.borrow_mut().push(if disabled {
                            format!("[ {label} ]")
                        } else if focused {
                            format!("▶[ {label} ]")
                        } else {
                            format!("[ {label} ]")
                        });
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for (name, table_mode) in [
            ("list", false),
            ("table", true),
            ("radio", false),
            ("scrollview", false),
        ] {
            let is_radio = name == "radio";
            let is_scrollview = name == "scrollview";
            let sink = Rc::clone(&lines);
            let panel_id = id.to_owned();
            let selection_callbacks = Rc::clone(&self.panel_selection_callbacks);
            let activation_callbacks = Rc::clone(&self.panel_activation_callbacks);
            let selection_state = Rc::clone(&self.panel_selection_state);
            let selection_dirty = Rc::clone(&self.panel_selection_dirty);
            let active_selection = Rc::clone(&self.panel_active_selection);
            let collection_panel_size = Rc::clone(&self.panel_size);
            let command_callbacks = Rc::clone(&self.panel_command_callbacks);
            let key_commands = Rc::clone(&self.panel_key_commands);
            let node_menus = Rc::clone(&self.panel_node_menus);
            let action_regions = Rc::clone(&self.panel_action_regions);
            let radio_lines = Rc::clone(&self.panel_radio_lines);
            let radio_focused = Rc::clone(&self.panel_focused_radio);
            let scrollview_lines = Rc::clone(&self.panel_scrollview_lines);
            let scrollview_focused = Rc::clone(&self.panel_focused_scrollview);
            let selection_key = format!("{panel_id}:{name}");
            panel
                .set(
                    name,
                    self._lua
                        .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                            if let Some(LuaValue::Table(value)) = value {
                                active_selection
                                    .borrow_mut()
                                    .insert(panel_id.clone(), selection_key.clone());
                                let item_count = value
                                    .get::<Option<Table>>(if table_mode {
                                        "rows"
                                    } else if is_radio {
                                        "options"
                                    } else {
                                        "items"
                                    })?
                                    .map(|items| items.sequence_values::<LuaValue>().count())
                                    .unwrap_or(0);
                                if item_count > 0 {
                                    let configured_present = if is_radio {
                                        value.get::<Option<LuaValue>>("selected")?.is_some()
                                    } else {
                                        value.get::<Option<u64>>("selected")?.is_some()
                                    };
                                    let configured_index = if is_radio {
                                        radio_selected_index(&value)?.unwrap_or(0)
                                    } else {
                                        value
                                            .get::<Option<u64>>("selected")?
                                            .unwrap_or(1)
                                            .saturating_sub(1)
                                            as usize
                                    };
                                    let selected_index = {
                                        let mut states = selection_state.borrow_mut();
                                        let selected =
                                            states.entry(selection_key.clone()).or_insert((
                                                configured_index.min(item_count - 1),
                                                item_count,
                                            ));
                                        if !selection_dirty.borrow().contains(&selection_key) {
                                            selected.0 = configured_index.min(item_count - 1);
                                        }
                                        selected.1 = item_count;
                                        selected.0 = selected.0.min(item_count - 1);
                                        selected.0
                                    };
                                    if configured_present
                                        || selection_dirty.borrow().contains(&selection_key)
                                    {
                                        value.set("selected", (selected_index + 1) as u64)?;
                                    }
                                }
                                let selection_callback = if is_radio {
                                    value
                                        .get::<Option<Function>>("on_change")?
                                        .or(value.get::<Option<Function>>("on_select")?)
                                } else {
                                    value.get::<Option<Function>>("on_select")?
                                };
                                if let Some(callback) = selection_callback {
                                    let callback = if table_mode {
                                        callback
                                    } else {
                                        let items = value
                                            .get::<Option<Table>>(if is_radio {
                                                "options"
                                            } else {
                                                "items"
                                            })?
                                            .or_else(|| {
                                                value.get::<Option<Table>>("rows").ok().flatten()
                                            });
                                        lua.create_function(move |_, row: usize| {
                                            let item = items
                                                .as_ref()
                                                .and_then(|items| {
                                                    items
                                                        .get::<Option<LuaValue>>(row)
                                                        .ok()
                                                        .flatten()
                                                })
                                                .unwrap_or(LuaValue::Nil);
                                            callback.call::<()>(item)
                                        })?
                                    };
                                    let key = lua.create_registry_value(callback)?;
                                    if let Some(previous) = selection_callbacks
                                        .borrow_mut()
                                        .insert(panel_id.clone(), key)
                                    {
                                        let _ = lua.remove_registry_value(previous);
                                    }
                                }
                                if table_mode {
                                    if let Some(callback) =
                                        value.get::<Option<Function>>("on_activate")?
                                    {
                                        let key = lua.create_registry_value(callback)?;
                                        if let Some(previous) = activation_callbacks
                                            .borrow_mut()
                                            .insert(panel_id.clone(), key)
                                        {
                                            let _ = lua.remove_registry_value(previous);
                                        }
                                    }
                                }
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_command")?
                                {
                                    let callback = if table_mode {
                                        callback
                                    } else {
                                        let items = value
                                            .get::<Option<Table>>(if is_radio {
                                                "options"
                                            } else {
                                                "items"
                                            })?
                                            .or_else(|| {
                                                value.get::<Option<Table>>("rows").ok().flatten()
                                            });
                                        lua.create_function(
                                            move |_, (command, row): (String, usize)| {
                                                let item = items
                                                    .as_ref()
                                                    .and_then(|items| {
                                                        items
                                                            .get::<Option<LuaValue>>(row)
                                                            .ok()
                                                            .flatten()
                                                    })
                                                    .unwrap_or(LuaValue::Nil);
                                                callback.call::<()>((command, item))
                                            },
                                        )?
                                    };
                                    let key = lua.create_registry_value(callback)?;
                                    if let Some(previous) =
                                        command_callbacks.borrow_mut().insert(panel_id.clone(), key)
                                    {
                                        let _ = lua.remove_registry_value(previous);
                                    }
                                }
                                if !is_radio && !table_mode {
                                    if let Some(callback) =
                                        value.get::<Option<Function>>("on_command")?
                                    {
                                        let items = value.get::<Option<Table>>("items")?;
                                        let callback =
                                            lua.create_function(move |_, row: usize| {
                                                let item = items
                                                    .as_ref()
                                                    .and_then(|items| {
                                                        items
                                                            .get::<Option<LuaValue>>(row)
                                                            .ok()
                                                            .flatten()
                                                    })
                                                    .unwrap_or(LuaValue::Nil);
                                                callback.call::<()>(("activate", item))
                                            })?;
                                        let key = lua.create_registry_value(callback)?;
                                        if let Some(previous) = activation_callbacks
                                            .borrow_mut()
                                            .insert(panel_id.clone(), key)
                                        {
                                            let _ = lua.remove_registry_value(previous);
                                        }
                                    }
                                }
                                if !is_radio {
                                    let mut bindings = BTreeMap::new();
                                    if let Some(commands) =
                                        value.get::<Option<Table>>("key_commands")?
                                    {
                                        for pair in commands.pairs::<String, String>() {
                                            let (key, command) = pair?;
                                            bindings.insert(key, command);
                                        }
                                    }
                                    key_commands.borrow_mut().insert(panel_id.clone(), bindings);
                                    let node_menu = panel_node_menu_entries(lua, &value)?;
                                    if node_menu.is_empty() {
                                        node_menus.borrow_mut().remove(&panel_id);
                                    } else {
                                        node_menus.borrow_mut().insert(panel_id.clone(), node_menu);
                                    }
                                }
                                let width =
                                    usize::from(collection_panel_size.borrow().0.saturating_sub(2));
                                let line = sink.borrow().len();
                                if is_radio {
                                    radio_lines.borrow_mut().insert(panel_id.clone(), line);
                                } else if is_scrollview {
                                    scrollview_lines.borrow_mut().insert(panel_id.clone(), line);
                                }
                                let rendered = if is_radio {
                                    radio_widget_text(lua, &value)
                                } else {
                                    collection_widget_text_with_width(
                                        lua,
                                        LuaValue::Table(value.clone()),
                                        table_mode,
                                        Some(width),
                                    )
                                };
                                if !is_radio {
                                    register_panel_action_regions(
                                        lua,
                                        &value,
                                        table_mode,
                                        line,
                                        &panel_id,
                                        &action_regions,
                                    );
                                }
                                if (is_radio && radio_focused.borrow().contains_key(&panel_id))
                                    || (is_scrollview
                                        && scrollview_focused.borrow().contains_key(&panel_id))
                                {
                                    sink.borrow_mut().push(format!("▶{rendered}"));
                                } else {
                                    sink.borrow_mut().push(rendered);
                                }
                            }
                            Ok(())
                        })
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let tabs_sink = Rc::clone(&lines);
        let tabs_panel_id = id.to_owned();
        let tabs_selection_callbacks = Rc::clone(&self.panel_selection_callbacks);
        let tabs_selection_state = Rc::clone(&self.panel_selection_state);
        let tabs_selection_dirty = Rc::clone(&self.panel_selection_dirty);
        let tabs_active_selection = Rc::clone(&self.panel_active_selection);
        let tabs_lines = Rc::clone(&self.panel_tabs_lines);
        let tabs_focused = Rc::clone(&self.panel_focused_tabs);
        panel
            .set(
                "tabs",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            let selection_key = format!("{tabs_panel_id}:tabs");
                            tabs_active_selection
                                .borrow_mut()
                                .insert(tabs_panel_id.clone(), selection_key.clone());
                            let item_count = value
                                .get::<Option<Table>>("items")?
                                .map(|items| items.sequence_values::<LuaValue>().count())
                                .unwrap_or(0);
                            if item_count > 0 {
                                let configured = value.get::<Option<u64>>("selected")?;
                                let active = value.get::<Option<String>>("active")?;
                                let items = value.get::<Option<Table>>("items")?;
                                let configured_index = configured
                                    .map(|selected| selected.saturating_sub(1) as usize)
                                    .or_else(|| {
                                        active.as_ref().and_then(|active| {
                                            items.as_ref().and_then(|items| {
                                                items
                                                    .sequence_values::<LuaValue>()
                                                    .enumerate()
                                                    .find_map(|(index, item)| {
                                                        let LuaValue::Table(item) = item.ok()?
                                                        else {
                                                            return None;
                                                        };
                                                        (item
                                                            .get::<Option<String>>("id")
                                                            .ok()
                                                            .flatten()
                                                            .as_deref()
                                                            == Some(active.as_str()))
                                                        .then_some(index)
                                                    })
                                            })
                                        })
                                    })
                                    .unwrap_or(0);
                                let selected_index = {
                                    let mut states = tabs_selection_state.borrow_mut();
                                    let selected = states.entry(selection_key.clone()).or_insert((
                                        configured_index.min(item_count - 1),
                                        item_count,
                                    ));
                                    if !tabs_selection_dirty.borrow().contains(&selection_key) {
                                        selected.0 = configured_index.min(item_count - 1);
                                    }
                                    selected.1 = item_count;
                                    selected.0 = selected.0.min(item_count - 1);
                                    selected.0
                                };
                                if configured.is_some()
                                    || active.is_some()
                                    || tabs_selection_dirty.borrow().contains(&selection_key)
                                {
                                    value.set("selected", (selected_index + 1) as u64)?;
                                    if active.is_some() {
                                        if let Some(active_id) = items.as_ref().and_then(|items| {
                                            items
                                                .get::<Option<LuaValue>>(selected_index + 1)
                                                .ok()
                                                .flatten()
                                                .and_then(|item| match item {
                                                    LuaValue::Table(item) => item
                                                        .get::<Option<String>>("id")
                                                        .ok()
                                                        .flatten(),
                                                    LuaValue::String(value) => value
                                                        .to_str()
                                                        .ok()
                                                        .map(|value| value.to_owned()),
                                                    _ => None,
                                                })
                                        }) {
                                            value.set("active", active_id)?;
                                        }
                                    }
                                }
                            }
                            let callback =
                                value
                                    .get::<Option<Function>>("on_change")?
                                    .or(value.get::<Option<Function>>("on_select")?);
                            if let Some(callback) = callback {
                                let items = value.get::<Option<Table>>("items")?;
                                let callback = lua.create_function(move |_, row: usize| {
                                    let item = items
                                        .as_ref()
                                        .and_then(|items| {
                                            items.get::<Option<LuaValue>>(row).ok().flatten()
                                        })
                                        .unwrap_or(LuaValue::Nil);
                                    let tab_id = match item {
                                        LuaValue::Table(item) => item
                                            .get::<Option<String>>("id")
                                            .ok()
                                            .flatten()
                                            .unwrap_or_else(|| row.to_string()),
                                        LuaValue::String(value) => value.to_str().map_or_else(
                                            |_| row.to_string(),
                                            |value| value.to_owned(),
                                        ),
                                        _ => row.to_string(),
                                    };
                                    callback.call::<()>(tab_id)
                                })?;
                                let key = lua.create_registry_value(callback)?;
                                if let Some(previous) = tabs_selection_callbacks
                                    .borrow_mut()
                                    .insert(tabs_panel_id.clone(), key)
                                {
                                    let _ = lua.remove_registry_value(previous);
                                }
                            }
                            let line = tabs_sink.borrow().len();
                            tabs_lines.borrow_mut().insert(tabs_panel_id.clone(), line);
                            let rendered =
                                collection_widget_text(lua, LuaValue::Table(value), false);
                            if tabs_focused.borrow().contains_key(&tabs_panel_id) {
                                tabs_sink.borrow_mut().push(format!("▶{rendered}"));
                            } else {
                                tabs_sink.borrow_mut().push(rendered);
                            }
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let pager_sink = Rc::clone(&lines);
        let pager_selection_state = Rc::clone(&self.panel_selection_state);
        let pager_selection_dirty = Rc::clone(&self.panel_selection_dirty);
        let pager_active_selection = Rc::clone(&self.panel_active_selection);
        let pager_selection_callbacks = Rc::clone(&self.panel_selection_callbacks);
        let pager_panel_id = id.to_owned();
        panel
            .set(
                "pager",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            let offset = value
                                .get::<Option<u64>>("offset")
                                .ok()
                                .flatten()
                                .unwrap_or(0);
                            let limit = value
                                .get::<Option<u64>>("limit")
                                .ok()
                                .flatten()
                                .filter(|limit| *limit > 0)
                                .unwrap_or(1);
                            let total = value.get::<Option<u64>>("total").ok().flatten();
                            let page = value.get::<Option<u64>>("page").ok().flatten();
                            let pages = value
                                .get::<Option<u64>>("pages")
                                .ok()
                                .flatten()
                                .or_else(|| total.map(|total| total.div_ceil(limit)));
                            if let Some(pages) = pages.filter(|pages| *pages > 0) {
                                let selection_key = format!("{pager_panel_id}:pager");
                                pager_active_selection
                                    .borrow_mut()
                                    .insert(pager_panel_id.clone(), selection_key.clone());
                                let configured_page = page
                                    .map(|page| page.saturating_sub(1))
                                    .unwrap_or(offset / limit)
                                    .min(pages - 1)
                                    as usize;
                                let selected_page = {
                                    let mut states = pager_selection_state.borrow_mut();
                                    let selected = states
                                        .entry(selection_key.clone())
                                        .or_insert((configured_page, pages as usize));
                                    if !pager_selection_dirty.borrow().contains(&selection_key) {
                                        selected.0 = configured_page;
                                    }
                                    selected.1 = pages as usize;
                                    selected.0 = selected.0.min(pages as usize - 1);
                                    selected.0
                                };
                                value.set("offset", selected_page as u64 * limit)?;
                                let callback = value.get::<Option<Function>>("on_change")?;
                                if let Some(callback) = callback {
                                    let callback = lua.create_function(move |_, page: u64| {
                                        callback.call::<()>(
                                            page.saturating_sub(1).saturating_mul(limit),
                                        )
                                    })?;
                                    let key = lua.create_registry_value(callback)?;
                                    if let Some(previous) = pager_selection_callbacks
                                        .borrow_mut()
                                        .insert(pager_panel_id.clone(), key)
                                    {
                                        let _ = lua.remove_registry_value(previous);
                                    }
                                }
                                pager_sink.borrow_mut().push(pager_widget_text(
                                    lua,
                                    &value,
                                    selected_page as u64,
                                    pages,
                                    limit,
                                    total,
                                ));
                            } else if is_drawer {
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
        let progress_sink = Rc::clone(&lines);
        let progress_panel_size = Rc::clone(&self.panel_size);
        panel
            .set(
                "progress",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(value) = value {
                            let width =
                                usize::from(progress_panel_size.borrow().0.saturating_sub(2))
                                    .max(1);
                            progress_sink
                                .borrow_mut()
                                .push(progress_widget_text(lua, value, width));
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let image_sink = Rc::clone(&lines);
        panel
            .set(
                "image",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            image_sink.borrow_mut().push(image_widget_text(lua, &value));
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        for name in [
            "badge",
            "border",
            "box",
            "dialog",
            "hstack",
            "raw",
            "row_action",
            "title",
            "vstack",
        ] {
            let sink = Rc::clone(&lines);
            let is_title = name == "title";
            let title_menus = Rc::clone(&self.panel_title_menus);
            let title_callbacks = Rc::clone(&self.panel_title_callbacks);
            let title_panel_id = id.to_owned();
            panel
                .set(
                    name,
                    self._lua
                        .create_function(
                            move |lua, (panel, values): (Table, Variadic<LuaValue>)| {
                                if is_title && values.len() == 1 {
                                    if let Some(LuaValue::Table(value)) = values.first() {
                                        let entries = panel_menu_entries(lua, value, "menu")?;
                                        if entries.is_empty() {
                                            title_menus.borrow_mut().remove(&title_panel_id);
                                        } else {
                                            title_menus
                                                .borrow_mut()
                                                .insert(title_panel_id.clone(), entries);
                                        }
                                        if let Some(callback) =
                                            value.get::<Option<Function>>("on_menu")?
                                        {
                                            let key = lua.create_registry_value(callback)?;
                                            if let Some(previous) = title_callbacks
                                                .borrow_mut()
                                                .insert(title_panel_id.clone(), key)
                                            {
                                                let _ = lua.remove_registry_value(previous);
                                            }
                                        }
                                        sink.borrow_mut().push(title_widget_text(lua, value)?);
                                        return Ok(());
                                    }
                                }
                                if !values.is_empty() {
                                    let mut rendered_children = false;
                                    if let Some(LuaValue::Table(value)) = values.first() {
                                        if let Some(render) =
                                            value.get::<Option<Function>>("render")?
                                        {
                                            render.call::<()>(panel.clone())?;
                                            rendered_children = true;
                                        }
                                    }
                                    if !rendered_children {
                                        sink.borrow_mut().push(
                                            values
                                                .into_iter()
                                                .map(|value| lua_value_text(lua, value))
                                                .collect::<Vec<_>>()
                                                .join(" "),
                                        );
                                    }
                                }
                                Ok(())
                            },
                        )
                        .map_err(lua_error)?,
                )
                .map_err(lua_error)?;
        }
        let checkbox_sink = Rc::clone(&lines);
        let checkbox_widgets = Rc::clone(&self.dialog_widgets);
        let drawer_checkbox = Rc::clone(&self.drawer_checkbox);
        let panel_checkboxes = Rc::clone(&self.panel_checkboxes);
        let panel_checkbox_id = id.to_owned();
        panel
            .set(
                "checkbox",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<LuaValue>)| {
                        if let Some(LuaValue::Table(value)) = value {
                            let checked = value.get::<Option<bool>>("checked")?.unwrap_or(false);
                            let label = value
                                .get::<Option<String>>("label")?
                                .or_else(|| value.get::<Option<String>>("text").ok().flatten())
                                .unwrap_or_default();
                            let displayed_checked = if let Some(dialog_id) = dialog_id {
                                let mut widgets = checkbox_widgets.borrow_mut();
                                let state = widgets.entry(dialog_id).or_default();
                                if state.checkbox.is_none() {
                                    state.checkbox = Some(PluginDialogCheckboxState {
                                        checked,
                                        on_change: None,
                                    });
                                }
                                let displayed_checked = state
                                    .checkbox
                                    .as_ref()
                                    .map(|item| item.checked)
                                    .unwrap_or(checked);
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    if let Some(existing) = state
                                        .checkbox
                                        .as_mut()
                                        .and_then(|item| item.on_change.take())
                                    {
                                        let _ = lua.remove_registry_value(existing);
                                    }
                                    state
                                        .checkbox
                                        .as_mut()
                                        .expect("checkbox inserted")
                                        .on_change = Some(lua.create_registry_value(callback)?);
                                }
                                displayed_checked
                            } else if is_drawer {
                                let mut drawer_state = drawer_checkbox.borrow_mut();
                                let state = drawer_state
                                    .get_or_insert_with(PluginDrawerCheckboxState::default);
                                if !state.initialized {
                                    state.checked = checked;
                                    state.initialized = true;
                                }
                                state.line = checkbox_sink.borrow().len();
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    if let Some(existing) = state.on_change.take() {
                                        let _ = lua.remove_registry_value(existing);
                                    }
                                    state.on_change = Some(lua.create_registry_value(callback)?);
                                }
                                state.checked
                            } else {
                                let mut states = panel_checkboxes.borrow_mut();
                                let state = states.entry(panel_checkbox_id.clone()).or_default();
                                if !state.initialized {
                                    state.checked = checked;
                                    state.initialized = true;
                                }
                                state.line = checkbox_sink.borrow().len();
                                if let Some(existing) = state.on_change.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    state.on_change = Some(lua.create_registry_value(callback)?);
                                }
                                state.checked
                            };
                            let focused = dialog_id.is_some_and(|id| {
                                checkbox_widgets
                                    .borrow()
                                    .get(&id)
                                    .is_some_and(|state| state.focus == PluginDialogFocus::Checkbox)
                            }) || is_drawer
                                && dialog_id.is_none()
                                && drawer_checkbox
                                    .borrow()
                                    .as_ref()
                                    .is_some_and(|state| state.focused);
                            let focused = focused
                                || panel_checkboxes
                                    .borrow()
                                    .get(&panel_checkbox_id)
                                    .is_some_and(|state| state.focused);
                            checkbox_sink.borrow_mut().push(format!(
                                "{}[{}] {}",
                                if focused { "▶" } else { "" },
                                if displayed_checked { 'x' } else { ' ' },
                                label
                            ));
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let password_sink = Rc::clone(&lines);
        let password_widgets = Rc::clone(&self.dialog_widgets);
        let drawer_password = Rc::clone(&self.drawer_password);
        let panel_passwords = Rc::clone(&self.panel_passwords);
        let panel_password_id = id.to_owned();
        panel
            .set(
                "password",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<Table>)| {
                        if let Some(value) = value {
                            if let Some(dialog_id) = dialog_id {
                                let text = value
                                    .get::<Option<String>>("text")?
                                    .or_else(|| value.get::<Option<String>>("value").ok().flatten())
                                    .unwrap_or_default();
                                let mut widgets = password_widgets.borrow_mut();
                                let state = widgets.entry(dialog_id).or_default();
                                if !state.input_initialized {
                                    state.input_text = text;
                                    state.input_cursor = state.input_text.chars().count();
                                    state.input_initialized = true;
                                }
                                state.input_masked = true;
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    if let Some(existing) = state.input_on_change.take() {
                                        let _ = lua.remove_registry_value(existing);
                                    }
                                    state.input_on_change =
                                        Some(lua.create_registry_value(callback)?);
                                }
                                let focused = state.focus == PluginDialogFocus::Input;
                                password_sink.borrow_mut().push(format!(
                                    "{}❯ {}",
                                    if focused { "▶" } else { "" },
                                    "•".repeat(state.input_text.chars().count())
                                ));
                            } else if is_drawer {
                                let text = value
                                    .get::<Option<String>>("text")?
                                    .or_else(|| value.get::<Option<String>>("value").ok().flatten())
                                    .unwrap_or_default();
                                let mut state = drawer_password.borrow_mut();
                                let state =
                                    state.get_or_insert_with(PluginDrawerInputState::default);
                                if !state.initialized {
                                    state.text = text;
                                    state.cursor = state.text.chars().count();
                                    state.initialized = true;
                                }
                                state.cursor = state.cursor.min(state.text.chars().count());
                                state.line = password_sink.borrow().len();
                                if let Some(existing) = state.on_change.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    state.on_change = Some(lua.create_registry_value(callback)?);
                                }
                                let placeholder = value
                                    .get::<Option<String>>("placeholder")?
                                    .unwrap_or_default();
                                let displayed = if state.text.is_empty() && !placeholder.is_empty()
                                {
                                    format!("[{placeholder}]")
                                } else {
                                    "•".repeat(state.text.chars().count())
                                };
                                password_sink.borrow_mut().push(format!(
                                    "{}❯ {}",
                                    if state.focused { "▶" } else { "" },
                                    displayed
                                ));
                            } else {
                                let text = value
                                    .get::<Option<String>>("text")?
                                    .or_else(|| value.get::<Option<String>>("value").ok().flatten())
                                    .unwrap_or_default();
                                let mut states = panel_passwords.borrow_mut();
                                let state = states.entry(panel_password_id.clone()).or_default();
                                if !state.initialized {
                                    state.text = text;
                                    state.cursor = state.text.chars().count();
                                    state.initialized = true;
                                }
                                state.cursor = state.cursor.min(state.text.chars().count());
                                state.line = password_sink.borrow().len();
                                if let Some(existing) = state.on_change.take() {
                                    let _ = lua.remove_registry_value(existing);
                                }
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    state.on_change = Some(lua.create_registry_value(callback)?);
                                }
                                let placeholder = value
                                    .get::<Option<String>>("placeholder")?
                                    .unwrap_or_default();
                                let displayed = if state.text.is_empty() && !placeholder.is_empty()
                                {
                                    format!("[{}]", placeholder)
                                } else {
                                    "•".repeat(state.text.chars().count())
                                };
                                password_sink.borrow_mut().push(format!(
                                    "{}❯ {}",
                                    if state.focused { "▶" } else { "" },
                                    displayed
                                ));
                            }
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
        let dropdown_sink = Rc::clone(&lines);
        let dropdown_widgets = Rc::clone(&self.dialog_widgets);
        let drawer_dropdown = Rc::clone(&self.drawer_dropdown);
        let dropdown_selection_state = Rc::clone(&self.panel_selection_state);
        let dropdown_selection_dirty = Rc::clone(&self.panel_selection_dirty);
        let dropdown_active_selection = Rc::clone(&self.panel_active_selection);
        let dropdown_selection_callbacks = Rc::clone(&self.panel_selection_callbacks);
        let dropdown_lines = Rc::clone(&self.panel_dropdown_lines);
        let dropdown_focused = Rc::clone(&self.panel_focused_dropdown);
        let dropdown_panel_id = id.to_owned();
        panel
            .set(
                "dropdown",
                self._lua
                    .create_function(move |lua, (_panel, value): (Table, Option<Table>)| {
                        if let Some(value) = value {
                            if let Some(dialog_id) = dialog_id {
                                let options = value
                                    .get::<Option<Table>>("options")?
                                    .map(|options| {
                                        options
                                            .sequence_values::<LuaValue>()
                                            .flatten()
                                            .map(|item| collection_item_text(lua, item, false))
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default();
                                let selected = value
                                    .get::<Option<u64>>("selected")?
                                    .unwrap_or(1)
                                    .saturating_sub(1)
                                    as usize;
                                let on_change = value
                                    .get::<Option<Function>>("on_change")?
                                    .map(|callback| lua.create_registry_value(callback))
                                    .transpose()?;
                                dropdown_widgets
                                    .borrow_mut()
                                    .entry(dialog_id)
                                    .or_default()
                                    .dropdown = Some(PluginDialogDropdownState {
                                    selected: selected.min(options.len().saturating_sub(1)),
                                    options,
                                    on_change,
                                });
                            } else if is_drawer {
                                let options = value
                                    .get::<Option<Table>>("options")?
                                    .map(|options| {
                                        options
                                            .sequence_values::<LuaValue>()
                                            .flatten()
                                            .map(|item| collection_item_text(lua, item, false))
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default();
                                let selected = value
                                    .get::<Option<u64>>("selected")?
                                    .unwrap_or(1)
                                    .saturating_sub(1)
                                    as usize;
                                let mut state = drawer_dropdown.borrow_mut();
                                let state =
                                    state.get_or_insert_with(PluginDrawerDropdownState::default);
                                if state.options.is_empty() {
                                    state.options = options;
                                    state.selected =
                                        selected.min(state.options.len().saturating_sub(1));
                                } else {
                                    state.options = options;
                                    state.selected =
                                        state.selected.min(state.options.len().saturating_sub(1));
                                }
                                state.line = dropdown_sink.borrow().len();
                                if let Some(callback) =
                                    value.get::<Option<Function>>("on_change")?
                                {
                                    if let Some(existing) = state.on_change.take() {
                                        let _ = lua.remove_registry_value(existing);
                                    }
                                    state.on_change = Some(lua.create_registry_value(callback)?);
                                }
                                value.set("selected", (state.selected + 1) as u64)?;
                            } else {
                                let line = dropdown_sink.borrow().len();
                                dropdown_lines
                                    .borrow_mut()
                                    .insert(dropdown_panel_id.clone(), line);
                                let option_values = value.get::<Option<Table>>("options")?;
                                let selection_key = format!("{dropdown_panel_id}:dropdown");
                                dropdown_active_selection
                                    .borrow_mut()
                                    .insert(dropdown_panel_id.clone(), selection_key.clone());
                                let option_count = option_values
                                    .as_ref()
                                    .map(|items| {
                                        items.sequence_values::<LuaValue>().flatten().count()
                                    })
                                    .unwrap_or(0);
                                if option_count > 0 {
                                    let configured_index = value
                                        .get::<Option<u64>>("selected")?
                                        .unwrap_or(1)
                                        .saturating_sub(1)
                                        as usize;
                                    let selected_index = {
                                        let mut states = dropdown_selection_state.borrow_mut();
                                        let selected =
                                            states.entry(selection_key.clone()).or_insert((
                                                configured_index.min(option_count - 1),
                                                option_count,
                                            ));
                                        if !dropdown_selection_dirty
                                            .borrow()
                                            .contains(&selection_key)
                                        {
                                            selected.0 = configured_index.min(option_count - 1);
                                        }
                                        selected.1 = option_count;
                                        selected.0 = selected.0.min(option_count - 1);
                                        selected.0
                                    };
                                    value.set("selected", (selected_index + 1) as u64)?;
                                }
                                let on_change = value.get::<Option<Function>>("on_change")?;
                                let on_menu = value.get::<Option<Function>>("on_menu")?;
                                if on_change.is_some() || on_menu.is_some() {
                                    let options = option_values;
                                    let callback =
                                        lua.create_function(move |lua, row: usize| {
                                            let item = options
                                                .as_ref()
                                                .and_then(|items| {
                                                    items
                                                        .get::<Option<LuaValue>>(row)
                                                        .ok()
                                                        .flatten()
                                                })
                                                .unwrap_or(LuaValue::Nil);
                                            if let Some(callback) = on_menu.as_ref() {
                                                let command = match &item {
                                                    LuaValue::Table(item) => item
                                                        .get::<Option<String>>("command")?
                                                        .unwrap_or_else(|| {
                                                            lua_value_text(
                                                                lua,
                                                                LuaValue::Table(item.clone()),
                                                            )
                                                        }),
                                                    _ => lua_value_text(lua, item.clone()),
                                                };
                                                callback.call::<()>(command)
                                            } else if let Some(callback) = on_change.as_ref() {
                                                callback.call::<()>(item)
                                            } else {
                                                Ok(())
                                            }
                                        })?;
                                    let key = lua.create_registry_value(callback)?;
                                    if let Some(previous) = dropdown_selection_callbacks
                                        .borrow_mut()
                                        .insert(dropdown_panel_id.clone(), key)
                                    {
                                        let _ = lua.remove_registry_value(previous);
                                    }
                                }
                            }
                            let rendered = dropdown_widget_text(lua, value);
                            if dropdown_focused.borrow().get(&dropdown_panel_id).is_some() {
                                dropdown_sink.borrow_mut().push(format!("▶{rendered}"));
                            } else {
                                dropdown_sink.borrow_mut().push(rendered);
                            }
                        }
                        Ok(())
                    })
                    .map_err(lua_error)?,
            )
            .map_err(lua_error)?;
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

    pub fn activate_panel_selection(&self, id: &str, row: usize) -> AppResult<bool> {
        let Some(callback_key) = self.panel_selection_callbacks.borrow_mut().remove(id) else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&callback_key).map_err(lua_error)?;
        let result = callback
            .call::<()>(row.saturating_add(1))
            .map_err(|error| plugin_error(format!("panel selection callback failed: {error}")));
        let _ = self._lua.remove_registry_value(callback_key);
        result.map(|()| true)
    }

    pub fn activate_panel_button(&self, id: &str, line: usize) -> AppResult<bool> {
        let callback: Function = {
            let actions = self.panel_button_actions.borrow();
            let Some(callback_key) = actions.get(id).and_then(|actions| {
                actions
                    .iter()
                    .find(|(action_line, _)| *action_line == line)
                    .map(|(_, key)| key)
            }) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        self.clear_panel_focus(id);
        self.panel_focused_button
            .borrow_mut()
            .insert(id.to_owned(), line);
        callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("panel button callback failed: {error}")))?;
        Ok(true)
    }

    pub fn panel_button_key(
        &self,
        id: &str,
        key_name: &str,
        rune: Option<char>,
    ) -> AppResult<bool> {
        let Some(focused) = self.panel_focused_button.borrow().get(id).copied() else {
            return Ok(false);
        };
        if key_name != "Enter" && rune != Some(' ') {
            return Ok(false);
        }
        self.activate_panel_button(id, focused)
    }

    pub fn focus_panel_dropdown(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_dropdown_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        self.panel_focused_dropdown
            .borrow_mut()
            .insert(id.to_owned(), line);
        true
    }

    pub fn panel_dropdown_key(&self, id: &str, key_name: &str) -> AppResult<bool> {
        if !self.panel_focused_dropdown.borrow().contains_key(id) {
            return Ok(false);
        }
        match key_name {
            "Up" | "Left" => self.move_panel_selection(id, -1),
            "Down" | "Right" => self.move_panel_selection(id, 1),
            "Enter" => self.activate_panel_current_selection(id),
            _ => Ok(false),
        }
    }

    pub fn focus_panel_radio(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_radio_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        self.panel_focused_radio
            .borrow_mut()
            .insert(id.to_owned(), line);
        true
    }

    pub fn panel_radio_key(&self, id: &str, key_name: &str) -> AppResult<bool> {
        if !self.panel_focused_radio.borrow().contains_key(id) {
            return Ok(false);
        }
        match key_name {
            "Up" | "Left" => self.move_panel_selection(id, -1),
            "Down" | "Right" => self.move_panel_selection(id, 1),
            "Enter" => self.activate_panel_current_selection(id),
            _ => Ok(false),
        }
    }

    pub fn focus_panel_tabs(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_tabs_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        self.panel_focused_tabs
            .borrow_mut()
            .insert(id.to_owned(), line);
        true
    }

    pub fn panel_tabs_key(&self, id: &str, key_name: &str) -> AppResult<bool> {
        if !self.panel_focused_tabs.borrow().contains_key(id) {
            return Ok(false);
        }
        match key_name {
            "Up" | "Left" => self.move_panel_selection(id, -1),
            "Down" | "Right" => self.move_panel_selection(id, 1),
            "Enter" => self.activate_panel_current_selection(id),
            _ => Ok(false),
        }
    }

    pub fn focus_panel_scrollview(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_scrollview_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        self.panel_focused_scrollview
            .borrow_mut()
            .insert(id.to_owned(), line);
        true
    }

    pub fn panel_scrollview_key(&self, id: &str, key_name: &str) -> AppResult<bool> {
        if !self.panel_focused_scrollview.borrow().contains_key(id) {
            return Ok(false);
        }
        match key_name {
            "Up" | "Left" => self.move_panel_selection(id, -1),
            "Down" | "Right" => self.move_panel_selection(id, 1),
            "Enter" => self.activate_panel_current_selection(id),
            _ => Ok(false),
        }
    }

    pub fn cycle_panel_focus(&self, id: &str, backwards: bool) -> bool {
        let mut controls = Vec::new();
        if let Some(state) = self.panel_inputs.borrow().get(id) {
            controls.push((state.line, 0_u8));
        }
        if let Some(state) = self.panel_passwords.borrow().get(id) {
            controls.push((state.line, 1_u8));
        }
        if let Some(state) = self.panel_checkboxes.borrow().get(id) {
            controls.push((state.line, 2_u8));
        }
        if let Some(line) = self.panel_dropdown_lines.borrow().get(id) {
            controls.push((*line, 4_u8));
        }
        if let Some(line) = self.panel_radio_lines.borrow().get(id) {
            controls.push((*line, 5_u8));
        }
        if let Some(line) = self.panel_tabs_lines.borrow().get(id) {
            controls.push((*line, 6_u8));
        }
        if let Some(line) = self.panel_scrollview_lines.borrow().get(id) {
            controls.push((*line, 7_u8));
        }
        if let Some(actions) = self.panel_button_actions.borrow().get(id) {
            controls.extend(actions.iter().map(|(line, _)| (*line, 3_u8)));
        }
        if controls.is_empty() {
            return false;
        }
        controls.sort_unstable();
        controls.dedup();
        let current = controls.iter().position(|(line, kind)| match kind {
            0 => self
                .panel_inputs
                .borrow()
                .get(id)
                .is_some_and(|state| state.focused && state.line == *line),
            1 => self
                .panel_passwords
                .borrow()
                .get(id)
                .is_some_and(|state| state.focused && state.line == *line),
            2 => self
                .panel_checkboxes
                .borrow()
                .get(id)
                .is_some_and(|state| state.focused && state.line == *line),
            3 => self
                .panel_focused_button
                .borrow()
                .get(id)
                .is_some_and(|focused| focused == line),
            4 => self
                .panel_focused_dropdown
                .borrow()
                .get(id)
                .is_some_and(|focused| focused == line),
            5 => self
                .panel_focused_radio
                .borrow()
                .get(id)
                .is_some_and(|focused| focused == line),
            6 => self
                .panel_focused_tabs
                .borrow()
                .get(id)
                .is_some_and(|focused| focused == line),
            7 => self
                .panel_focused_scrollview
                .borrow()
                .get(id)
                .is_some_and(|focused| focused == line),
            _ => false,
        });
        let next = match (current, backwards) {
            (Some(index), false) => (index + 1) % controls.len(),
            (Some(index), true) => (index + controls.len() - 1) % controls.len(),
            (None, false) => 0,
            (None, true) => controls.len() - 1,
        };
        for state in self.panel_inputs.borrow_mut().values_mut() {
            state.focused = false;
        }
        for state in self.panel_passwords.borrow_mut().values_mut() {
            state.focused = false;
        }
        for state in self.panel_checkboxes.borrow_mut().values_mut() {
            state.focused = false;
        }
        self.panel_focused_button.borrow_mut().remove(id);
        self.panel_focused_dropdown.borrow_mut().remove(id);
        self.panel_focused_radio.borrow_mut().remove(id);
        self.panel_focused_tabs.borrow_mut().remove(id);
        self.panel_focused_scrollview.borrow_mut().remove(id);
        let (line, kind) = controls[next];
        match kind {
            0 => {
                if let Some(state) = self.panel_inputs.borrow_mut().get_mut(id) {
                    state.focused = true;
                }
            }
            1 => {
                if let Some(state) = self.panel_passwords.borrow_mut().get_mut(id) {
                    state.focused = true;
                }
            }
            2 => {
                if let Some(state) = self.panel_checkboxes.borrow_mut().get_mut(id) {
                    state.focused = true;
                }
            }
            3 => {
                self.panel_focused_button
                    .borrow_mut()
                    .insert(id.to_owned(), line);
            }
            4 => {
                self.panel_focused_dropdown
                    .borrow_mut()
                    .insert(id.to_owned(), line);
            }
            5 => {
                self.panel_focused_radio
                    .borrow_mut()
                    .insert(id.to_owned(), line);
            }
            6 => {
                self.panel_focused_tabs
                    .borrow_mut()
                    .insert(id.to_owned(), line);
            }
            7 => {
                self.panel_focused_scrollview
                    .borrow_mut()
                    .insert(id.to_owned(), line);
            }
            _ => return false,
        }
        true
    }

    pub fn activate_panel_action(&self, id: &str, x: usize, y: usize) -> AppResult<bool> {
        let action = {
            let mut regions = self.panel_action_regions.borrow_mut();
            let Some(actions) = regions.get_mut(id) else {
                return Ok(false);
            };
            let Some(index) = actions
                .iter()
                .position(|action| action.line == y && action.start <= x && x < action.end)
            else {
                return Ok(false);
            };
            actions.remove(index)
        };
        let callback: Function = {
            let callbacks = self.panel_command_callbacks.borrow();
            let Some(key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(key).map_err(lua_error)?
        };
        callback
            .call::<()>((action.command, action.row))
            .map_err(|error| plugin_error(format!("panel action callback failed: {error}")))?;
        Ok(true)
    }

    /// Focus or activate the interactive control rendered on a panel row.
    ///
    /// Mouse dispatch already has a separate path for row actions. This
    /// helper covers the controls whose callbacks are otherwise only
    /// reachable through keyboard focus, so a click on an input, checkbox,
    /// selector, or button has the same semantics as Tab/Enter navigation.
    pub fn activate_panel_control_at(&self, id: &str, line: usize) -> AppResult<bool> {
        if self
            .panel_inputs
            .borrow()
            .get(id)
            .is_some_and(|state| state.line == line)
        {
            return Ok(self.focus_panel_input(id, line));
        }
        if self
            .panel_passwords
            .borrow()
            .get(id)
            .is_some_and(|state| state.line == line)
        {
            return Ok(self.focus_panel_password(id, line));
        }
        if self
            .panel_checkboxes
            .borrow()
            .get(id)
            .is_some_and(|state| state.line == line)
        {
            return self.activate_panel_checkbox(id, line);
        }
        if self
            .panel_dropdown_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line)
        {
            return Ok(self.focus_panel_dropdown(id, line));
        }
        if self
            .panel_radio_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line)
        {
            return Ok(self.focus_panel_radio(id, line));
        }
        if self
            .panel_tabs_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line)
        {
            return Ok(self.focus_panel_tabs(id, line));
        }
        if self
            .panel_scrollview_lines
            .borrow()
            .get(id)
            .is_some_and(|registered| *registered == line)
        {
            return Ok(self.focus_panel_scrollview(id, line));
        }
        if self
            .panel_button_actions
            .borrow()
            .get(id)
            .is_some_and(|actions| actions.iter().any(|(registered, _)| *registered == line))
        {
            return self.activate_panel_button(id, line);
        }
        Ok(false)
    }

    fn clear_panel_focus(&self, id: &str) {
        if let Some(state) = self.panel_inputs.borrow_mut().get_mut(id) {
            state.focused = false;
        }
        if let Some(state) = self.panel_passwords.borrow_mut().get_mut(id) {
            state.focused = false;
        }
        if let Some(state) = self.panel_checkboxes.borrow_mut().get_mut(id) {
            state.focused = false;
        }
        self.panel_focused_button.borrow_mut().remove(id);
        self.panel_focused_dropdown.borrow_mut().remove(id);
        self.panel_focused_radio.borrow_mut().remove(id);
        self.panel_focused_tabs.borrow_mut().remove(id);
        self.panel_focused_scrollview.borrow_mut().remove(id);
    }

    pub fn focus_panel_input(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_inputs
            .borrow()
            .get(id)
            .is_some_and(|state| state.line == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        let mut inputs = self.panel_inputs.borrow_mut();
        let state = inputs
            .get_mut(id)
            .expect("panel input exists after focus check");
        state.focused = true;
        true
    }

    pub fn focus_autofocus_input(&self, id: &str) -> bool {
        let line = self
            .panel_inputs
            .borrow()
            .get(id)
            .filter(|state| state.autofocus)
            .map(|state| state.line);
        line.is_some_and(|line| self.focus_panel_input(id, line))
    }

    pub fn panel_input_key(
        &self,
        id: &str,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let (text, change, submit, clear_on_submit) = {
            let mut inputs = self.panel_inputs.borrow_mut();
            let Some(state) = inputs.get_mut(id) else {
                return Ok(false);
            };
            if !state.focused || (!modifier.is_empty() && key_name != "Backspace") {
                return Ok(false);
            }
            if key_name == "Enter" && !state.multiline {
                let submit = state
                    .on_submit
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?;
                (state.text.clone(), None, submit, state.clear_on_submit)
            } else {
                let Some(changed) = edit_plugin_input(
                    &mut state.text,
                    &mut state.cursor,
                    key_name,
                    rune,
                    state.multiline,
                ) else {
                    return Ok(false);
                };
                let change = if changed {
                    state
                        .on_change
                        .as_ref()
                        .map(|key| self._lua.registry_value::<Function>(key))
                        .transpose()
                        .map_err(lua_error)?
                } else {
                    None
                };
                (state.text.clone(), change, None, false)
            }
        };
        if let Some(callback) = submit {
            callback
                .call::<()>(text)
                .map_err(|error| plugin_error(format!("panel input submit failed: {error}")))?;
            if clear_on_submit {
                if let Some(state) = self.panel_inputs.borrow_mut().get_mut(id) {
                    state.text.clear();
                    state.cursor = 0;
                }
            }
        } else if let Some(callback) = change {
            callback
                .call::<()>(text)
                .map_err(|error| plugin_error(format!("panel input change failed: {error}")))?;
        }
        Ok(true)
    }

    pub fn focus_panel_password(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_passwords
            .borrow()
            .get(id)
            .is_some_and(|state| state.line == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        let mut passwords = self.panel_passwords.borrow_mut();
        let state = passwords
            .get_mut(id)
            .expect("panel password exists after focus check");
        state.focused = true;
        true
    }

    pub fn panel_password_key(
        &self,
        id: &str,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let (text, callback) = {
            let mut passwords = self.panel_passwords.borrow_mut();
            let Some(state) = passwords.get_mut(id) else {
                return Ok(false);
            };
            if !state.focused || (!modifier.is_empty() && key_name != "Backspace") {
                return Ok(false);
            }
            if key_name != "Enter" {
                let Some(changed) =
                    edit_plugin_input(&mut state.text, &mut state.cursor, key_name, rune, false)
                else {
                    return Ok(false);
                };
                if !changed {
                    return Ok(true);
                }
            }
            let callback = state
                .on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?;
            (state.text.clone(), callback)
        };
        if let Some(callback) = callback {
            callback
                .call::<()>(text)
                .map_err(|error| plugin_error(format!("panel password change failed: {error}")))?;
        }
        Ok(true)
    }

    pub fn focus_panel_checkbox(&self, id: &str, line: usize) -> bool {
        let matches = self
            .panel_checkboxes
            .borrow()
            .get(id)
            .is_some_and(|state| state.line == line);
        if !matches {
            return false;
        }
        self.clear_panel_focus(id);
        let mut checkboxes = self.panel_checkboxes.borrow_mut();
        let state = checkboxes
            .get_mut(id)
            .expect("panel checkbox exists after focus check");
        state.focused = true;
        true
    }

    pub fn activate_panel_checkbox(&self, id: &str, line: usize) -> AppResult<bool> {
        if !self.focus_panel_checkbox(id, line) {
            return Ok(false);
        }
        self.toggle_panel_checkbox(id)
    }

    pub fn panel_checkbox_key(
        &self,
        id: &str,
        key_name: &str,
        rune: Option<char>,
    ) -> AppResult<bool> {
        let should_toggle = self
            .panel_checkboxes
            .borrow()
            .get(id)
            .is_some_and(|state| state.focused)
            && (key_name == "Enter" || rune == Some(' '));
        if !should_toggle {
            return Ok(false);
        }
        self.toggle_panel_checkbox(id)
    }

    fn toggle_panel_checkbox(&self, id: &str) -> AppResult<bool> {
        let (checked, callback) = {
            let mut checkboxes = self.panel_checkboxes.borrow_mut();
            let Some(state) = checkboxes.get_mut(id) else {
                return Ok(false);
            };
            state.checked = !state.checked;
            let callback = state
                .on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?;
            (state.checked, callback)
        };
        if let Some(callback) = callback {
            callback.call::<()>(checked).map_err(|error| {
                plugin_error(format!("panel checkbox callback failed: {error}"))
            })?;
        }
        Ok(true)
    }

    pub fn activate_drawer_line(&self, line: usize) -> AppResult<bool> {
        let (index, callback): (usize, Function) = {
            let actions = self.drawer_actions.borrow();
            let Some((index, (_, callback_key))) = actions
                .iter()
                .enumerate()
                .find(|(_, (action_line, _))| *action_line == line)
            else {
                return Ok(false);
            };
            (
                index,
                self._lua.registry_value(callback_key).map_err(lua_error)?,
            )
        };
        *self.drawer_focused_button.borrow_mut() = Some(index);
        let result = callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("drawer button callback failed: {error}")));
        result.map(|()| true)
    }

    fn clear_drawer_focus(&self) {
        if let Some(state) = self.drawer_input.borrow_mut().as_mut() {
            state.focused = false;
        }
        if let Some(state) = self.drawer_password.borrow_mut().as_mut() {
            state.focused = false;
        }
        if let Some(state) = self.drawer_checkbox.borrow_mut().as_mut() {
            state.focused = false;
        }
        if let Some(state) = self.drawer_dropdown.borrow_mut().as_mut() {
            state.focused = false;
        }
        *self.drawer_focused_button.borrow_mut() = None;
    }

    pub fn focus_drawer_line(&self, line: usize) -> bool {
        let input = self
            .drawer_input
            .borrow()
            .as_ref()
            .is_some_and(|state| state.line == line);
        let password = self
            .drawer_password
            .borrow()
            .as_ref()
            .is_some_and(|state| state.line == line);
        let checkbox = self
            .drawer_checkbox
            .borrow()
            .as_ref()
            .is_some_and(|state| state.line == line);
        let dropdown = self
            .drawer_dropdown
            .borrow()
            .as_ref()
            .is_some_and(|state| state.line == line);
        let button = self
            .drawer_actions
            .borrow()
            .iter()
            .position(|(action_line, _)| *action_line == line);
        if !(input || password || checkbox || dropdown || button.is_some()) {
            return false;
        }
        self.clear_drawer_focus();
        if input {
            if let Some(state) = self.drawer_input.borrow_mut().as_mut() {
                state.focused = true;
            }
        } else if password {
            if let Some(state) = self.drawer_password.borrow_mut().as_mut() {
                state.focused = true;
            }
        } else if checkbox {
            if let Some(state) = self.drawer_checkbox.borrow_mut().as_mut() {
                state.focused = true;
            }
        } else if dropdown {
            if let Some(state) = self.drawer_dropdown.borrow_mut().as_mut() {
                state.focused = true;
            }
        } else if let Some(index) = button {
            *self.drawer_focused_button.borrow_mut() = Some(index);
        }
        true
    }

    pub fn cycle_drawer_focus(&self, backwards: bool) -> bool {
        let mut controls = Vec::new();
        if let Some(state) = self.drawer_input.borrow().as_ref() {
            controls.push((state.line, 0_u8, 0_usize));
        }
        if let Some(state) = self.drawer_password.borrow().as_ref() {
            controls.push((state.line, 1_u8, 0_usize));
        }
        if let Some(state) = self.drawer_checkbox.borrow().as_ref() {
            controls.push((state.line, 2_u8, 0));
        }
        if let Some(state) = self.drawer_dropdown.borrow().as_ref() {
            controls.push((state.line, 3_u8, 0));
        }
        controls.extend(
            self.drawer_actions
                .borrow()
                .iter()
                .enumerate()
                .map(|(index, (line, _))| (*line, 4_u8, index)),
        );
        if controls.is_empty() {
            return false;
        }
        controls.sort_unstable();
        let current = controls.iter().position(|(_, kind, index)| match kind {
            0 => self
                .drawer_input
                .borrow()
                .as_ref()
                .is_some_and(|state| state.focused && *kind == 0 && *index == 0),
            1 => self
                .drawer_password
                .borrow()
                .as_ref()
                .is_some_and(|state| state.focused && *kind == 1),
            2 => self
                .drawer_checkbox
                .borrow()
                .as_ref()
                .is_some_and(|state| state.focused && *kind == 2),
            3 => self
                .drawer_dropdown
                .borrow()
                .as_ref()
                .is_some_and(|state| state.focused && *kind == 3),
            4 => self
                .drawer_focused_button
                .borrow()
                .is_some_and(|focused| focused == *index),
            _ => false,
        });
        let next = match (current, backwards) {
            (Some(index), false) => (index + 1) % controls.len(),
            (Some(index), true) => (index + controls.len() - 1) % controls.len(),
            (None, false) => 0,
            (None, true) => controls.len() - 1,
        };
        self.clear_drawer_focus();
        let (line, kind, index) = controls[next];
        match kind {
            0 => self.focus_drawer_line(line),
            1 => self.focus_drawer_line(line),
            2 => self.focus_drawer_line(line),
            3 => self.focus_drawer_line(line),
            4 => {
                *self.drawer_focused_button.borrow_mut() = Some(index);
                true
            }
            _ => false,
        }
    }

    pub fn clear_drawer_state(&self) {
        *self.drawer_focused_button.borrow_mut() = None;
        for (_, callback) in self.drawer_actions.borrow_mut().drain(..) {
            let _ = self._lua.remove_registry_value(callback);
        }
        if let Some(mut state) = self.drawer_input.borrow_mut().take() {
            if let Some(callback) = state.on_change.take() {
                let _ = self._lua.remove_registry_value(callback);
            }
            if let Some(callback) = state.on_submit.take() {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        if let Some(mut state) = self.drawer_checkbox.borrow_mut().take() {
            if let Some(callback) = state.on_change.take() {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        if let Some(mut state) = self.drawer_password.borrow_mut().take() {
            if let Some(callback) = state.on_change.take() {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        if let Some(mut state) = self.drawer_dropdown.borrow_mut().take() {
            if let Some(callback) = state.on_change.take() {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
    }

    pub fn drawer_input_key(
        &self,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let focused_button = self.drawer_focused_button.borrow().as_ref().copied();
        if let Some(index) = focused_button {
            if key_name != "Enter" && rune != Some(' ') {
                return Ok(false);
            }
            let line = self
                .drawer_actions
                .borrow()
                .get(index)
                .map(|(line, _)| *line);
            let Some(line) = line else {
                return Ok(false);
            };
            return self.activate_drawer_line(line);
        }
        if self
            .drawer_dropdown
            .borrow()
            .as_ref()
            .is_some_and(|state| state.focused)
        {
            let delta = match key_name {
                "Up" | "Left" => Some(-1),
                "Down" | "Right" => Some(1),
                "Enter" => Some(0),
                _ => None,
            };
            let Some(delta) = delta else {
                return Ok(false);
            };
            let (selected, callback) = {
                let mut dropdown = self.drawer_dropdown.borrow_mut();
                let state = dropdown.as_mut().expect("focused drawer dropdown exists");
                if !state.options.is_empty() {
                    state.selected = (state.selected as isize + delta)
                        .rem_euclid(state.options.len() as isize)
                        as usize;
                }
                let callback = state
                    .on_change
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?;
                (state.selected + 1, callback)
            };
            if let Some(callback) = callback {
                callback.call::<()>(selected).map_err(|error| {
                    plugin_error(format!("drawer dropdown change failed: {error}"))
                })?;
            }
            return Ok(true);
        }
        if self
            .drawer_checkbox
            .borrow()
            .as_ref()
            .is_some_and(|state| state.focused)
        {
            if key_name != "Enter" && rune != Some(' ') {
                return Ok(false);
            }
            let (checked, callback) = {
                let mut checkbox = self.drawer_checkbox.borrow_mut();
                let state = checkbox.as_mut().expect("focused drawer checkbox exists");
                state.checked = !state.checked;
                let callback = state
                    .on_change
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?;
                (state.checked, callback)
            };
            if let Some(callback) = callback {
                callback.call::<()>(checked).map_err(|error| {
                    plugin_error(format!("drawer checkbox change failed: {error}"))
                })?;
            }
            return Ok(true);
        }
        let (text, callback, submit, clear_on_submit) = {
            let mut input = self.drawer_input.borrow_mut();
            let Some(state) = input.as_mut() else {
                return Ok(false);
            };
            if !state.focused || !modifier.is_empty() && key_name != "Backspace" {
                return Ok(false);
            }
            let submit = key_name == "Enter" && !state.multiline;
            if !submit {
                let Some(changed) = edit_plugin_input(
                    &mut state.text,
                    &mut state.cursor,
                    key_name,
                    rune,
                    state.multiline,
                ) else {
                    return Ok(false);
                };
                if !changed {
                    return Ok(true);
                }
            }
            let callback = if submit {
                state
                    .on_submit
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?
            } else {
                state
                    .on_change
                    .as_ref()
                    .map(|key| self._lua.registry_value::<Function>(key))
                    .transpose()
                    .map_err(lua_error)?
            };
            (state.text.clone(), callback, submit, state.clear_on_submit)
        };
        if let Some(callback) = callback {
            callback.call::<()>(text).map_err(|error| {
                plugin_error(if submit {
                    format!("drawer input submit failed: {error}")
                } else {
                    format!("drawer input change failed: {error}")
                })
            })?;
        }
        if submit && clear_on_submit {
            if let Some(state) = self.drawer_input.borrow_mut().as_mut() {
                state.text.clear();
                state.cursor = 0;
            }
        }
        Ok(true)
    }

    pub fn drawer_password_key(
        &self,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let (text, callback) = {
            let mut password = self.drawer_password.borrow_mut();
            let Some(state) = password.as_mut() else {
                return Ok(false);
            };
            if !state.focused || (!modifier.is_empty() && key_name != "Backspace") {
                return Ok(false);
            }
            if key_name == "Enter" {
                return Ok(true);
            }
            let Some(changed) =
                edit_plugin_input(&mut state.text, &mut state.cursor, key_name, rune, false)
            else {
                return Ok(false);
            };
            if !changed {
                return Ok(true);
            }
            let callback = state
                .on_change
                .as_ref()
                .map(|key| self._lua.registry_value::<Function>(key))
                .transpose()
                .map_err(lua_error)?;
            (state.text.clone(), callback)
        };
        if let Some(callback) = callback {
            callback
                .call::<()>(text)
                .map_err(|error| plugin_error(format!("drawer password change failed: {error}")))?;
        }
        Ok(true)
    }

    pub fn activate_panel_row(&self, id: &str, row: usize) -> AppResult<bool> {
        let Some(callback_key) = self.panel_activation_callbacks.borrow_mut().remove(id) else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(&callback_key).map_err(lua_error)?;
        let result = callback
            .call::<()>(row.saturating_add(1))
            .map_err(|error| plugin_error(format!("panel row activation failed: {error}")));
        let _ = self._lua.remove_registry_value(callback_key);
        result.map(|()| true)
    }

    pub fn activate_status_item(&self, id: &str) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.status_callbacks.borrow();
            let Some(callback_key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        let result = callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("status item callback failed: {error}")));
        result.map(|()| true)
    }

    pub fn open_file(&self, path: &Path) -> AppResult<Option<String>> {
        *self.last_file_close_id.borrow_mut() = None;
        *self.last_file_title.borrow_mut() = None;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        let file_prefix = if let Ok(mut file) = fs::File::open(path) {
            let mut prefix = vec![0; 512];
            let length = file.read(&mut prefix).unwrap_or(0);
            prefix.truncate(length);
            prefix
        } else {
            Vec::new()
        };
        let handlers = self.registration.borrow().file_handlers.clone();
        let handler = handlers
            .iter()
            .find(|handler| {
                handler
                    .magic
                    .as_ref()
                    .is_some_and(|magic| !magic.is_empty() && file_prefix.starts_with(magic))
            })
            .or_else(|| {
                extension.as_deref().and_then(|extension| {
                    handlers.iter().find(|handler| {
                        handler.extensions.iter().any(|candidate| {
                            candidate
                                .trim_start_matches('.')
                                .eq_ignore_ascii_case(extension)
                        })
                    })
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
        let panel_requests = Rc::clone(&self.panel_requests);
        tab.set(
            "focus",
            self._lua
                .create_function(move |_, _tab: Table| {
                    panel_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::FocusFileView);
                    Ok(())
                })
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
        let panel_requests = Rc::clone(&self.panel_requests);
        tab.set(
            "close",
            self._lua
                .create_function(move |_, _tab: Table| {
                    panel_requests
                        .borrow_mut()
                        .push(PluginPanelRequest::CloseFileView);
                    Ok(())
                })
                .map_err(lua_error)?,
        )
        .map_err(lua_error)?;
        let returned = callback
            .call::<LuaValue>((path.to_string_lossy().to_string(), tab.clone()))
            .map_err(lua_error)?;
        *self.last_file_title.borrow_mut() = Some(tab_title.borrow().clone());
        let close = tab.get::<Option<Function>>("on_close").map_err(lua_error)?;
        let on_event = tab.get::<Option<Function>>("on_event").map_err(lua_error)?;
        if close.is_some() || on_event.is_some() {
            let mut next = self.next_file_close_id.borrow_mut();
            *next = next.saturating_add(1);
            let id = *next;
            if let Some(close) = close {
                let key = self._lua.create_registry_value(close).map_err(lua_error)?;
                self.file_close_callbacks.borrow_mut().insert(id, key);
            }
            if let Some(on_event) = on_event {
                let key = self
                    ._lua
                    .create_registry_value(on_event)
                    .map_err(lua_error)?;
                self.file_event_callbacks.borrow_mut().insert(id, key);
            }
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
        let file_id = self.last_file_close_id.borrow().unwrap_or_else(|| {
            let mut next = self.next_file_close_id.borrow_mut();
            *next = next.saturating_add(1);
            *next
        });
        *self.last_file_close_id.borrow_mut() = Some(file_id);
        let render_key = self
            ._lua
            .create_registry_value(render.clone())
            .map_err(lua_error)?;
        self.panel_callbacks
            .borrow_mut()
            .insert(format!("__file:{file_id}"), render_key);
        // File views use the same full panel proxy as registered panels and
        // custom tabs. This keeps widget state, mouse hit testing, and
        // redraw semantics identical across all plugin-owned views.
        self.render_panel(&format!("__file:{file_id}"))
    }

    pub fn take_file_close_id(&self) -> Option<u64> {
        self.last_file_close_id.borrow_mut().take()
    }

    pub fn take_file_title(&self) -> Option<String> {
        self.last_file_title.borrow_mut().take()
    }

    pub fn close_file(&self, id: u64) -> AppResult<()> {
        if let Some(render_key) = self
            .panel_callbacks
            .borrow_mut()
            .remove(&format!("__file:{id}"))
        {
            let _ = self._lua.remove_registry_value(render_key);
        }
        let close_key = self.file_close_callbacks.borrow_mut().remove(&id);
        if let Some(event_key) = self.file_event_callbacks.borrow_mut().remove(&id) {
            let _ = self._lua.remove_registry_value(event_key);
        }
        let Some(key) = close_key else {
            return Ok(());
        };
        let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
        let result = callback.call::<()>(()).map_err(lua_error);
        let _ = self._lua.remove_registry_value(key);
        result
    }

    pub fn render_file_view(&self, id: u64) -> AppResult<Option<String>> {
        self.render_panel(&format!("__file:{id}"))
    }

    pub fn emit_file_key_event(
        &self,
        id: u64,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.file_event_callbacks.borrow();
            let Some(callback_key) = callbacks.get(&id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
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
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] file tab on_event failed: {error}"));
                Ok(false)
            }
        }
    }

    pub fn emit_file_mouse_event(&self, id: u64, x: u16, y: u16, button: &str) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.file_event_callbacks.borrow();
            let Some(callback_key) = callbacks.get(&id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        let event = self._lua.create_table().map_err(lua_error)?;
        event.set("type", "mouse").map_err(lua_error)?;
        event.set("x", x).map_err(lua_error)?;
        event.set("y", y).map_err(lua_error)?;
        event.set("button", button).map_err(lua_error)?;
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] file tab on_event failed: {error}"));
                Ok(false)
            }
        }
    }

    pub fn emit_tab_key_event(
        &self,
        id: &str,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.tab_event_callbacks.borrow();
            let Some(callback_key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
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
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] tab on_event failed: {error}"));
                Ok(false)
            }
        }
    }

    pub fn emit_tab_mouse_event(&self, id: &str, x: u16, y: u16, button: &str) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.tab_event_callbacks.borrow();
            let Some(callback_key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        let event = self._lua.create_table().map_err(lua_error)?;
        event.set("type", "mouse").map_err(lua_error)?;
        event.set("x", x).map_err(lua_error)?;
        event.set("y", y).map_err(lua_error)?;
        event.set("button", button).map_err(lua_error)?;
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] tab on_event failed: {error}"));
                Ok(false)
            }
        }
    }

    pub fn emit_panel_key_event(
        &self,
        id: &str,
        key_name: &str,
        rune: Option<char>,
        modifier: &str,
    ) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.panel_event_callbacks.borrow();
            let Some(callback_key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
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
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] panel on_event failed: {error}"));
                Ok(false)
            }
        }
    }

    pub fn emit_panel_mouse_event(
        &self,
        id: &str,
        x: u16,
        y: u16,
        button: &str,
    ) -> AppResult<bool> {
        let callback: Function = {
            let callbacks = self.panel_event_callbacks.borrow();
            let Some(callback_key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        let event = self._lua.create_table().map_err(lua_error)?;
        event.set("type", "mouse").map_err(lua_error)?;
        event.set("x", x).map_err(lua_error)?;
        event.set("y", y).map_err(lua_error)?;
        event.set("button", button).map_err(lua_error)?;
        match callback.call::<bool>(event) {
            Ok(handled) => Ok(handled),
            Err(error) => {
                self.logs
                    .borrow_mut()
                    .push(format!("[error] panel on_event failed: {error}"));
                Ok(false)
            }
        }
    }

    pub fn move_panel_selection(&self, id: &str, delta: isize) -> AppResult<bool> {
        let Some(selection_key) = self.panel_active_selection.borrow().get(id).cloned() else {
            return Ok(false);
        };
        let next = {
            let mut states = self.panel_selection_state.borrow_mut();
            let Some((selected, count)) = states.get_mut(&selection_key) else {
                return Ok(false);
            };
            if *count == 0 {
                return Ok(false);
            }
            let next = if delta.is_negative() {
                selected.saturating_sub(delta.unsigned_abs())
            } else {
                selected.saturating_add(delta as usize).min(*count - 1)
            };
            if next == *selected {
                return Ok(false);
            }
            *selected = next;
            next
        };
        self.panel_selection_dirty
            .borrow_mut()
            .insert(selection_key);
        let _ = self.activate_panel_selection(id, next)?;
        Ok(true)
    }

    pub fn move_panel_horizontal_selection(&self, id: &str, delta: isize) -> AppResult<bool> {
        let is_pager = self
            .panel_active_selection
            .borrow()
            .get(id)
            .is_some_and(|selection_key| selection_key.ends_with(":pager"));
        if !is_pager {
            return Ok(false);
        }
        self.move_panel_selection(id, delta)
    }

    pub fn move_tree_horizontal_selection(&self, id: &str, expand: bool) -> AppResult<bool> {
        let Some(selection_key) = self.panel_active_selection.borrow().get(id).cloned() else {
            return Ok(false);
        };
        if !selection_key.ends_with(":tree") {
            return Ok(false);
        }
        let Some(row) = self
            .panel_selection_state
            .borrow()
            .get(&selection_key)
            .map(|(selected, _)| selected + 1)
        else {
            return Ok(false);
        };
        let callbacks = self.tree_navigation_callbacks.borrow();
        let Some(callback_key) = callbacks.get(id) else {
            return Ok(false);
        };
        let callback: Function = self._lua.registry_value(callback_key).map_err(lua_error)?;
        let changed = callback
            .call::<bool>((row, expand))
            .map_err(|error| plugin_error(format!("tree navigation callback failed: {error}")))?;
        if changed {
            self.panel_selection_dirty
                .borrow_mut()
                .insert(selection_key);
        }
        Ok(changed)
    }

    pub fn activate_panel_current_selection(&self, id: &str) -> AppResult<bool> {
        let Some(selection_key) = self.panel_active_selection.borrow().get(id).cloned() else {
            return Ok(false);
        };
        let Some(selected) = self
            .panel_selection_state
            .borrow()
            .get(&selection_key)
            .map(|(selected, _)| *selected)
        else {
            return Ok(false);
        };
        let mut activated = false;
        if self.panel_selection_callbacks.borrow().contains_key(id) {
            activated |= self.activate_panel_selection(id, selected)?;
        }
        if self.panel_activation_callbacks.borrow().contains_key(id) {
            activated |= self.activate_panel_row(id, selected)?;
        }
        Ok(activated)
    }

    pub fn activate_panel_command(&self, id: &str, key: &str) -> AppResult<bool> {
        let Some(command) = self
            .panel_key_commands
            .borrow()
            .get(id)
            .and_then(|commands| commands.get(key))
            .cloned()
        else {
            return Ok(false);
        };
        let Some(selection_key) = self.panel_active_selection.borrow().get(id).cloned() else {
            return Ok(false);
        };
        let Some(row) = self
            .panel_selection_state
            .borrow()
            .get(&selection_key)
            .map(|(selected, _)| selected + 1)
        else {
            return Ok(false);
        };
        let callback: Function = {
            let callbacks = self.panel_command_callbacks.borrow();
            let Some(callback_key) = callbacks.get(id) else {
                return Ok(false);
            };
            self._lua.registry_value(callback_key).map_err(lua_error)?
        };
        callback
            .call::<()>((command, row))
            .map_err(|error| plugin_error(format!("panel command callback failed: {error}")))?;
        Ok(true)
    }

    pub fn close_tab(&self, id: &str) -> AppResult<bool> {
        let callback = self.open_tabs.borrow_mut().remove(id);
        let tab_exists = callback.is_some();
        for callbacks in [
            &self.panel_selection_callbacks,
            &self.panel_activation_callbacks,
            &self.panel_command_callbacks,
            &self.panel_title_callbacks,
        ] {
            if let Some(callback) = callbacks.borrow_mut().remove(id) {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        if let Some(actions) = self.panel_button_actions.borrow_mut().remove(id) {
            for (_, callback) in actions {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        if let Some(selection_key) = self.panel_active_selection.borrow_mut().remove(id) {
            self.panel_selection_state
                .borrow_mut()
                .remove(&selection_key);
        }
        self.panel_title_menus.borrow_mut().remove(id);
        self.panel_key_commands.borrow_mut().remove(id);
        self.panel_focused_button.borrow_mut().remove(id);
        self.panel_dropdown_lines.borrow_mut().remove(id);
        self.panel_action_regions.borrow_mut().remove(id);
        self.panel_radio_lines.borrow_mut().remove(id);
        self.panel_tabs_lines.borrow_mut().remove(id);
        self.panel_scrollview_lines.borrow_mut().remove(id);
        for states in [&self.panel_inputs, &self.panel_passwords] {
            if let Some(mut state) = states.borrow_mut().remove(id) {
                for callback in [state.on_change.take(), state.on_submit.take()]
                    .into_iter()
                    .flatten()
                {
                    let _ = self._lua.remove_registry_value(callback);
                }
            }
        }
        if let Some(state) = self.panel_checkboxes.borrow_mut().remove(id) {
            if let Some(callback) = state.on_change {
                let _ = self._lua.remove_registry_value(callback);
            }
        }
        if let Some(render_key) = self
            .panel_callbacks
            .borrow_mut()
            .remove(&format!("__tab:{id}"))
        {
            let _ = self._lua.remove_registry_value(render_key);
        }
        if let Some(event_key) = self.tab_event_callbacks.borrow_mut().remove(id) {
            let _ = self._lua.remove_registry_value(event_key);
        }
        let Some(key) = callback.flatten() else {
            return Ok(tab_exists);
        };
        let callback: Function = self._lua.registry_value(&key).map_err(lua_error)?;
        let result = callback
            .call::<()>(())
            .map_err(|error| plugin_error(format!("tab on_close failed: {error}")));
        let _ = self._lua.remove_registry_value(key);
        result.map(|()| true)
    }
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        for flag in self.async_cancel.borrow().values() {
            flag.store(true, Ordering::Relaxed);
        }
        let handles = std::mem::take(&mut *self.spawn_handles.borrow_mut());
        let children = handles
            .into_values()
            .filter_map(|handle| handle.lock().ok().map(|handle| Arc::clone(&handle.child)))
            .collect::<Vec<_>>();
        for child in children {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
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
    panel_requests: Rc<RefCell<Vec<PluginPanelRequest>>>,
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
        let focus_requests = Rc::clone(&panel_requests);
        module
            .set(
                "focus",
                lua.create_function(move |_, id: String| {
                    if focus_manager.borrow().sessions.contains_key(&id) {
                        focus_requests
                            .borrow_mut()
                            .push(PluginPanelRequest::FocusTerminal);
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
    max_output_bytes: Option<usize>,
}

type AsyncCommandValues = (
    Vec<String>,
    CommandOptions,
    Option<Function>,
    Option<Function>,
);

fn command_allowed(argv: &[String], allowed: &[String], allow_all: bool) -> bool {
    if allow_all {
        return true;
    }
    argv.first()
        .is_some_and(|command| allowed.iter().any(|allowed| allowed == command))
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
            let max_output_bytes = table
                .get::<Option<u64>>("max_output_bytes")?
                .map(|value| {
                    usize::try_from(value)
                        .map_err(|_| mlua::Error::external("max_output_bytes is too large"))
                })
                .transpose()?;
            if max_output_bytes == Some(0) {
                return Err(mlua::Error::external("max_output_bytes must be at least 1"));
            }
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
                max_output_bytes,
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

fn parse_async_command_values(values: Vec<LuaValue>) -> mlua::Result<AsyncCommandValues> {
    let output_callbacks = match values.get(2) {
        Some(LuaValue::Table(options)) => Some((
            options.get::<Option<Function>>("on_stdout")?,
            options.get::<Option<Function>>("on_stderr")?,
        )),
        _ => None,
    };
    let (argv, options) = parse_command_values(values)?;
    let (on_stdout, on_stderr) = output_callbacks.unwrap_or((None, None));
    Ok((argv, options, on_stdout, on_stderr))
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
) -> mlua::Result<(std::process::Output, bool, bool)> {
    run_command_cancellable(argv, options, None)
        .map(|(output, timed_out, _, truncated)| (output, timed_out, truncated))
}

fn read_command_output<R: Read>(
    mut reader: R,
    limit: Option<usize>,
    total: Arc<std::sync::atomic::AtomicUsize>,
    truncated: Arc<AtomicBool>,
    output_sender: Option<(Sender<AsyncEvent>, u64, bool)>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let used = total.fetch_add(read, Ordering::SeqCst);
        let keep = limit.map_or(read, |limit| limit.saturating_sub(used).min(read));
        output.extend_from_slice(&buffer[..keep]);
        if keep > 0 {
            if let Some((sender, id, stderr)) = &output_sender {
                let _ = sender.send(AsyncEvent::Output {
                    id: *id,
                    stderr: *stderr,
                    data: String::from_utf8_lossy(&buffer[..keep]).into_owned(),
                });
            }
        }
        if keep < read {
            truncated.store(true, Ordering::SeqCst);
            break;
        }
    }
    Ok(output)
}

fn run_command_cancellable(
    argv: &[String],
    options: &CommandOptions,
    cancel: Option<&AtomicBool>,
) -> mlua::Result<(std::process::Output, bool, bool, bool)> {
    run_command_cancellable_with_output(argv, options, cancel, None)
}

fn run_command_cancellable_with_output(
    argv: &[String],
    options: &CommandOptions,
    cancel: Option<&AtomicBool>,
    output_sender: Option<(Sender<AsyncEvent>, u64)>,
) -> mlua::Result<(std::process::Output, bool, bool, bool)> {
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
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| mlua::Error::external("stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| mlua::Error::external("stderr pipe unavailable"))?;
    let output_total = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let output_truncated = Arc::new(AtomicBool::new(false));
    let stdout_total = Arc::clone(&output_total);
    let stdout_truncated = Arc::clone(&output_truncated);
    let stdout_limit = options.max_output_bytes;
    let stdout_sender = output_sender
        .as_ref()
        .map(|(sender, id)| (sender.clone(), *id, false));
    let stdout_reader = thread::spawn(move || {
        read_command_output(
            stdout,
            stdout_limit,
            stdout_total,
            stdout_truncated,
            stdout_sender,
        )
    });
    let stderr_total = Arc::clone(&output_total);
    let stderr_truncated = Arc::clone(&output_truncated);
    let stderr_limit = options.max_output_bytes;
    let stderr_sender = output_sender
        .as_ref()
        .map(|(sender, id)| (sender.clone(), *id, true));
    let stderr_reader = thread::spawn(move || {
        read_command_output(
            stderr,
            stderr_limit,
            stderr_total,
            stderr_truncated,
            stderr_sender,
        )
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
    let mut cancelled = false;
    let mut truncated = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(mlua::Error::external)? {
            break status;
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            cancelled = true;
            child.kill().map_err(mlua::Error::external)?;
            break child.wait().map_err(mlua::Error::external)?;
        }
        if output_truncated.load(Ordering::SeqCst) {
            truncated = true;
            child.kill().map_err(mlua::Error::external)?;
            break child.wait().map_err(mlua::Error::external)?;
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
        cancelled,
        truncated || output_truncated.load(Ordering::SeqCst),
    ))
}

fn command_result(
    lua: &Lua,
    output: std::process::Output,
    timed_out: bool,
    cancelled: bool,
    truncated: bool,
) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set(
        "stdout",
        String::from_utf8_lossy(&output.stdout).to_string(),
    )?;
    result.set(
        "stderr",
        String::from_utf8_lossy(&output.stderr).to_string(),
    )?;
    let interrupted = timed_out || cancelled || truncated;
    result.set(
        "exit_code",
        if interrupted {
            -1
        } else {
            output.status.code().unwrap_or(-1)
        },
    )?;
    result.set("ok", !interrupted && output.status.success())?;
    result.set("timed_out", timed_out)?;
    result.set("cancelled", cancelled)?;
    result.set("truncated", truncated)?;
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

fn lua_value_usize(value: Option<&LuaValue>) -> Option<usize> {
    match value {
        Some(LuaValue::Integer(value)) if *value >= 0 => (*value).try_into().ok(),
        Some(LuaValue::Number(value)) if value.is_finite() && *value >= 0.0 => {
            Some(*value as usize)
        }
        _ => None,
    }
}

fn raw_panel_text(lines: &mut Vec<String>, x: usize, y: usize, text: &str) {
    for (offset, part) in text.split('\n').enumerate() {
        let line_index = y + offset;
        while lines.len() <= line_index {
            lines.push(String::new());
        }
        let mut chars = lines[line_index].chars().collect::<Vec<_>>();
        if chars.len() < x {
            chars.resize(x, ' ');
        }
        for (index, character) in part.chars().enumerate() {
            let target = x + index;
            if chars.len() <= target {
                chars.resize(target + 1, ' ');
            }
            chars[target] = character;
        }
        lines[line_index] = chars.into_iter().collect();
    }
}

fn clear_panel_region(lines: &mut Vec<String>, x: usize, y: usize, width: usize, height: usize) {
    if width == 0 || height == 0 {
        return;
    }
    for line_index in y..y.saturating_add(height) {
        while lines.len() <= line_index {
            lines.push(String::new());
        }
        let mut chars = lines[line_index].chars().collect::<Vec<_>>();
        let end = x.saturating_add(width);
        if chars.len() < end {
            chars.resize(end, ' ');
        }
        for character in chars.iter_mut().take(end).skip(x) {
            *character = ' ';
        }
        lines[line_index] = chars.into_iter().collect();
    }
}

fn panel_node_menu_entries(_lua: &Lua, value: &Table) -> mlua::Result<Vec<(String, String)>> {
    panel_menu_entries(_lua, value, "node_menu")
}

fn panel_menu_entries(
    _lua: &Lua,
    value: &Table,
    field: &str,
) -> mlua::Result<Vec<(String, String)>> {
    let Some(entries) = value.get::<Option<Table>>(field)? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for item in entries.sequence_values::<Table>() {
        let item = item?;
        if item.get::<Option<bool>>("separator")?.unwrap_or(false) {
            result.push(("────────".to_owned(), String::new()));
            continue;
        }
        let Some(label) = item.get::<Option<String>>("label")? else {
            continue;
        };
        let command = item
            .get::<Option<String>>("command")?
            .unwrap_or_else(|| label.clone());
        let label = match item.get::<Option<bool>>("checked")? {
            Some(true) => format!("✓ {label}"),
            Some(false) => format!("  {label}"),
            None => label,
        };
        result.push((label, command));
    }
    Ok(result)
}

fn tree_visible_nodes(table: &Table) -> Vec<LuaValue> {
    let mut nodes = Vec::new();
    let Some(items) = table.get::<Option<Table>>("items").ok().flatten() else {
        return nodes;
    };
    tree_visible_nodes_from_items(&items, &mut nodes);
    nodes
}

fn tree_visible_nodes_from_items(items: &Table, nodes: &mut Vec<LuaValue>) {
    for item in items.sequence_values::<Table>().flatten() {
        nodes.push(LuaValue::Table(item.clone()));
        let expanded = item
            .get::<Option<bool>>("expanded")
            .ok()
            .flatten()
            .unwrap_or(false);
        if expanded {
            if let Some(children) = item.get::<Option<Table>>("children").ok().flatten() {
                tree_visible_nodes_from_items(&children, nodes);
            }
        }
    }
}

fn tree_visible_nodes_with_depth(table: &Table) -> Vec<(LuaValue, usize)> {
    let mut nodes = Vec::new();
    let Some(items) = table.get::<Option<Table>>("items").ok().flatten() else {
        return nodes;
    };
    tree_visible_nodes_with_depth_from_items(&items, 0, &mut nodes);
    nodes
}

fn tree_visible_nodes_with_depth_from_items(
    items: &Table,
    depth: usize,
    nodes: &mut Vec<(LuaValue, usize)>,
) {
    for item in items.sequence_values::<Table>().flatten() {
        nodes.push((LuaValue::Table(item.clone()), depth));
        let expanded = item
            .get::<Option<bool>>("expanded")
            .ok()
            .flatten()
            .unwrap_or(false);
        if expanded {
            if let Some(children) = item.get::<Option<Table>>("children").ok().flatten() {
                tree_visible_nodes_with_depth_from_items(&children, depth + 1, nodes);
            }
        }
    }
}

fn tree_widget_text_with_width(lua: &Lua, table: &Table, width: Option<usize>) -> String {
    let loading = table.get::<Option<bool>>("loading").ok().flatten() == Some(true);
    if loading {
        return "⏳ loading".to_owned();
    }
    let nodes = tree_visible_nodes_with_depth(table);
    if nodes.is_empty() {
        return table
            .get::<Option<String>>("empty")
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    let selected = table
        .get::<Option<u64>>("selected")
        .ok()
        .flatten()
        .and_then(|value| value.checked_sub(1))
        .map(|value| value as usize);
    let indent = table
        .get::<Option<u64>>("indent")
        .ok()
        .flatten()
        .unwrap_or(2)
        .min(16) as usize;
    let inset = table
        .get::<Option<u64>>("inset")
        .ok()
        .flatten()
        .unwrap_or(0)
        .min(64) as usize;
    nodes
        .into_iter()
        .enumerate()
        .map(|(index, (value, depth))| {
            let has_children = matches!(&value, LuaValue::Table(item) if item
                .get::<Option<Table>>("children")
                .ok()
                .flatten()
                .is_some_and(|children| children.sequence_values::<LuaValue>().next().is_some()));
            let expanded = matches!(&value, LuaValue::Table(item) if item
                .get::<Option<bool>>("expanded")
                .ok()
                .flatten()
                .unwrap_or(false));
            let branch = if has_children {
                if expanded {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };
            let badge_right = table.get::<Option<bool>>("badge_right").ok().flatten() == Some(true);
            let text = if badge_right {
                collection_item_text_with_badge(lua, value.clone(), false, false)
            } else {
                collection_item_text(lua, value.clone(), false)
            };
            let text = if badge_right {
                let badge = match value {
                    LuaValue::Table(item) => item
                        .get::<Option<String>>("badge")
                        .ok()
                        .flatten()
                        .filter(|badge| !badge.is_empty()),
                    _ => None,
                };
                right_badge_text(text, badge.as_deref(), width)
            } else {
                text
            };
            let marker = if selected == Some(index) {
                "▶"
            } else {
                branch
            };
            let prefix = format!("{}{} ", " ".repeat(inset + depth * indent), marker);
            let text = if table.get::<Option<bool>>("truncate_left").ok().flatten() == Some(true) {
                width
                    .map(|width| {
                        truncate_left_text(&text, width.saturating_sub(prefix.chars().count()))
                    })
                    .unwrap_or(text)
            } else {
                text
            };
            format!("{prefix}{text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn collection_widget_text(lua: &Lua, value: LuaValue, table_mode: bool) -> String {
    collection_widget_text_with_width(lua, value, table_mode, None)
}

fn collection_widget_text_with_width(
    lua: &Lua,
    value: LuaValue,
    table_mode: bool,
    width: Option<usize>,
) -> String {
    let LuaValue::Table(table) = value else {
        return lua_value_text(lua, value);
    };
    let loading = table.get::<Option<bool>>("loading").ok().flatten() == Some(true);
    if loading && !table_mode {
        return "⏳ loading".to_owned();
    }
    let key = if table_mode { "rows" } else { "items" };
    let Ok(items) = table.get::<Option<Table>>(key) else {
        return lua_value_text(lua, LuaValue::Table(table));
    };
    let Some(items) = items else {
        if loading {
            return "⏳ loading".to_owned();
        }
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
    let inset = table
        .get::<Option<u64>>("inset")
        .ok()
        .flatten()
        .unwrap_or(0)
        .min(64) as usize;
    let values: Vec<_> = items.sequence_values::<LuaValue>().flatten().collect();
    if values.is_empty() {
        if loading {
            return "⏳ loading".to_owned();
        }
        return table
            .get::<Option<String>>("empty")
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    if table_mode
        && table
            .get::<Option<Table>>("columns")
            .ok()
            .flatten()
            .is_some()
    {
        let rendered = table_widget_text_with_columns(lua, &table, &values, selected, width);
        if loading {
            let mut lines = rendered.lines();
            let header = lines.next().unwrap_or_default();
            return std::iter::once(format!("{header}  ⏳ loading"))
                .chain(lines.map(str::to_owned))
                .collect::<Vec<_>>()
                .join("\n");
        }
        return rendered;
    }
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let badge_right = !table_mode
                && table.get::<Option<bool>>("badge_right").ok().flatten() == Some(true);
            let text = if badge_right {
                collection_item_text_with_badge(lua, value.clone(), table_mode, false)
            } else {
                collection_item_text(lua, value.clone(), table_mode)
            };
            let text = if badge_right {
                let badge = match value {
                    LuaValue::Table(item) => item
                        .get::<Option<String>>("badge")
                        .ok()
                        .flatten()
                        .filter(|badge| !badge.is_empty()),
                    _ => None,
                };
                right_badge_text(text, badge.as_deref(), width)
            } else {
                text
            };
            let text = format!("{}{}", " ".repeat(inset), text);
            let text = if !table_mode
                && table.get::<Option<bool>>("truncate_left").ok().flatten() == Some(true)
            {
                width
                    .map(|width| truncate_left_text(&text, width.saturating_sub(2)))
                    .unwrap_or(text)
            } else {
                text
            };
            let text = if table_mode {
                horizontal_table_slice(&table, &text)
            } else {
                text
            };
            if selected == Some(index) {
                format!("▶ {text}")
            } else {
                format!("  {text}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone)]
struct TableColumnSpec {
    label: String,
    width: usize,
    fixed: bool,
    align: String,
    actions: Option<String>,
}

fn table_widget_text_with_columns(
    lua: &Lua,
    table: &Table,
    rows: &[LuaValue],
    selected: Option<usize>,
    width: Option<usize>,
) -> String {
    let Some(columns) = table.get::<Option<Table>>("columns").ok().flatten() else {
        return String::new();
    };
    let mut specs = columns
        .sequence_values::<Table>()
        .flatten()
        .map(|column| {
            let label = column
                .get::<Option<String>>("label")
                .ok()
                .flatten()
                .unwrap_or_default();
            let fixed = column.get::<Option<u64>>("width").ok().flatten();
            let min_width = column
                .get::<Option<u64>>("min_width")
                .ok()
                .flatten()
                .unwrap_or_else(|| UnicodeWidthStr::width(label.as_str()) as u64);
            let label_width = UnicodeWidthStr::width(label.as_str());
            let actions = column
                .get::<Option<Table>>("actions")
                .ok()
                .flatten()
                .map(|actions| {
                    actions
                        .sequence_values::<LuaValue>()
                        .flatten()
                        .filter_map(|action| match action {
                            LuaValue::String(label) => {
                                label.to_str().ok().map(|text| text.to_owned())
                            }
                            LuaValue::Table(action) => action
                                .get::<Option<String>>("label")
                                .ok()
                                .flatten()
                                .or_else(|| {
                                    action
                                        .get::<Option<bool>>("menu")
                                        .ok()
                                        .flatten()
                                        .filter(|menu| *menu)
                                        .map(|_| "⋮".to_owned())
                                }),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .filter(|text| !text.is_empty());
            let action_width = actions.as_deref().map(UnicodeWidthStr::width).unwrap_or(0);
            let fixed_width = fixed.map(|value| value as usize);
            TableColumnSpec {
                label,
                width: fixed_width
                    .unwrap_or_else(|| (min_width as usize).max(label_width).max(action_width))
                    .max(1),
                fixed: fixed_width.is_some(),
                align: column
                    .get::<Option<String>>("align")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "left".to_owned()),
                actions,
            }
        })
        .collect::<Vec<_>>();
    if specs.is_empty() {
        return rows
            .iter()
            .enumerate()
            .map(|(index, row)| table_row_line(lua, row, selected == Some(index)))
            .collect::<Vec<_>>()
            .join("\n");
    }
    // The closure above needs the column index while retaining a compact parser.
    // Recompute auto widths with the actual index; this also handles short rows.
    for (index, spec) in specs.iter_mut().enumerate() {
        if spec.fixed {
            continue;
        }
        let natural = rows
            .iter()
            .filter_map(|row| table_row_cells(lua, row).get(index).cloned())
            .map(|cell| UnicodeWidthStr::width(cell.as_str()))
            .max()
            .unwrap_or(0);
        spec.width = spec.width.max(natural);
    }
    let gap = 2 * specs.len().saturating_sub(1);
    let pinned_action_width = specs
        .last()
        .and_then(|spec| spec.actions.as_ref().map(|_| spec.width));
    let lines = std::iter::once(format_table_row(
        &specs,
        &specs.iter().map(|c| c.label.clone()).collect::<Vec<_>>(),
        false,
    ))
    .chain(rows.iter().enumerate().map(|(index, row)| {
        let mut cells = table_row_cells(lua, row);
        for (column, spec) in specs.iter().enumerate() {
            if column >= cells.len() {
                cells.push(String::new());
            }
            if let Some(actions) = &spec.actions {
                cells[column] = actions.clone();
            }
        }
        format_table_row(&specs, &cells, selected == Some(index))
    }))
    .collect::<Vec<_>>();
    lines
        .into_iter()
        .map(|line| {
            if let Some(available) = width.filter(|available| *available > 0) {
                if UnicodeWidthStr::width(line.as_str()) + gap > available {
                    if let Some(action_width) = pinned_action_width {
                        return horizontal_table_slice_with_pinned_action(
                            table,
                            &line,
                            available,
                            action_width,
                        );
                    }
                    return horizontal_table_slice(table, &line);
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Kept separate from the parser so array rows and `{cells = ...}` rows share semantics.
fn table_row_cells(lua: &Lua, row: &LuaValue) -> Vec<String> {
    let LuaValue::Table(row) = row else {
        return vec![lua_value_text(lua, row.clone())];
    };
    row.get::<Option<Table>>("cells")
        .ok()
        .flatten()
        .map(|cells| {
            cells
                .sequence_values::<LuaValue>()
                .flatten()
                .map(|cell| lua_value_text(lua, cell))
                .collect()
        })
        .unwrap_or_else(|| {
            row.sequence_values::<LuaValue>()
                .flatten()
                .map(|cell| lua_value_text(lua, cell))
                .collect()
        })
}

fn format_table_row(specs: &[TableColumnSpec], cells: &[String], selected: bool) -> String {
    let body = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let cell = cells.get(index).map(String::as_str).unwrap_or("");
            let truncated = truncate_right_text(cell, spec.width);
            let padding = spec
                .width
                .saturating_sub(UnicodeWidthStr::width(truncated.as_str()));
            match spec.align.as_str() {
                "right" => format!("{}{}", " ".repeat(padding), truncated),
                "center" => {
                    let left = padding / 2;
                    format!(
                        "{}{}{}",
                        " ".repeat(left),
                        truncated,
                        " ".repeat(padding - left)
                    )
                }
                _ => format!("{}{}", truncated, " ".repeat(padding)),
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    if selected {
        format!("▶ {body}")
    } else {
        format!("  {body}")
    }
}

fn table_row_line(lua: &Lua, row: &LuaValue, selected: bool) -> String {
    let text = table_row_cells(lua, row).join("  ");
    if selected {
        format!("▶ {text}")
    } else {
        format!("  {text}")
    }
}

fn right_badge_text(text: String, badge: Option<&str>, width: Option<usize>) -> String {
    let Some(badge) = badge else {
        return text;
    };
    let Some(width) = width else {
        return format!("{text}  {badge}");
    };
    let available = width.saturating_sub(2);
    let text_width = UnicodeWidthStr::width(text.as_str());
    let badge_width = UnicodeWidthStr::width(badge);
    if text_width + 2 + badge_width <= available {
        return format!(
            "{text}{}{}",
            " ".repeat(available - text_width - badge_width),
            badge
        );
    }
    let remaining = available.saturating_sub(text_width + 2);
    if remaining < 3 {
        text
    } else {
        format!("{text}  {}", truncate_right_text(badge, remaining))
    }
}

fn truncate_right_text(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width <= UnicodeWidthStr::width("…") {
        return "…".chars().take(width).collect();
    }
    let keep_width = width - UnicodeWidthStr::width("…");
    let mut used = 0;
    let prefix = text
        .chars()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if used + character_width > keep_width {
                return false;
            }
            used += character_width;
            true
        })
        .collect::<String>();
    format!("{prefix}…")
}

fn truncate_left_text(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width <= UnicodeWidthStr::width("…") {
        return "…".chars().take(width).collect();
    }
    let keep_width = width - UnicodeWidthStr::width("…");
    let mut used = 0;
    let mut tail = text
        .chars()
        .rev()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if used + character_width > keep_width {
                return false;
            }
            used += character_width;
            true
        })
        .collect::<Vec<_>>();
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

fn radio_selected_index(table: &Table) -> mlua::Result<Option<usize>> {
    let selected = table.get::<Option<LuaValue>>("selected")?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let Some(options) = table.get::<Option<Table>>("options")? else {
        return Ok(None);
    };
    match selected {
        LuaValue::Integer(value) if value > 0 => Ok(Some(value as usize - 1)),
        LuaValue::Number(value) if value > 0.0 => Ok(Some(value as usize - 1)),
        LuaValue::String(value) => {
            let value = value.to_str()?.to_owned();
            Ok(options
                .sequence_values::<LuaValue>()
                .enumerate()
                .find_map(|(index, item)| {
                    let item = item.ok()?;
                    let matches = match item {
                        LuaValue::String(item) => {
                            item.to_str().ok().is_some_and(|item| item == value)
                        }
                        LuaValue::Table(item) => ["id", "value", "label"].into_iter().any(|key| {
                            item.get::<Option<String>>(key)
                                .ok()
                                .flatten()
                                .is_some_and(|item| item == value)
                        }),
                        _ => false,
                    };
                    matches.then_some(index)
                }))
        }
        _ => Ok(None),
    }
}

fn radio_widget_text(lua: &Lua, table: &Table) -> String {
    if table.get::<Option<bool>>("loading").ok().flatten() == Some(true) {
        return "⏳ loading".to_owned();
    }
    let Some(options) = table.get::<Option<Table>>("options").ok().flatten() else {
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
    let values: Vec<_> = options.sequence_values::<LuaValue>().flatten().collect();
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
            let marker = if selected == Some(index) {
                "◉"
            } else {
                "○"
            };
            format!("{marker} {}", collection_item_text(lua, value, false))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn horizontal_table_slice(table: &Table, text: &str) -> String {
    let offset = table
        .get::<Option<u64>>("scroll_x")
        .ok()
        .flatten()
        .or_else(|| table.get::<Option<u64>>("horizontal_scroll").ok().flatten())
        .unwrap_or(0) as usize;
    let width = table
        .get::<Option<u64>>("width")
        .ok()
        .flatten()
        .map(|value| value as usize);
    let chars: Vec<char> = text.chars().collect();
    let start = offset.min(chars.len());
    let end = width.map_or(chars.len(), |width| {
        start.saturating_add(width).min(chars.len())
    });
    chars[start..end].iter().collect()
}

fn horizontal_table_slice_with_pinned_action(
    table: &Table,
    text: &str,
    width: usize,
    action_width: usize,
) -> String {
    let suffix_width = action_width.saturating_add(2);
    if width <= suffix_width {
        return display_suffix(text, width);
    }
    let suffix = display_suffix(text, suffix_width);
    let prefix = &text[..text.len().saturating_sub(suffix.len())];
    let visible_prefix = horizontal_table_slice_to_width(table, prefix, width - suffix_width);
    format!("{visible_prefix}{suffix}")
}

fn horizontal_table_slice_to_width(table: &Table, text: &str, width: usize) -> String {
    let offset = table
        .get::<Option<u64>>("scroll_x")
        .ok()
        .flatten()
        .or_else(|| table.get::<Option<u64>>("horizontal_scroll").ok().flatten())
        .unwrap_or(0) as usize;
    let chars: Vec<char> = text.chars().collect();
    let start = offset.min(chars.len());
    let end = start.saturating_add(width).min(chars.len());
    chars[start..end].iter().collect()
}

fn display_suffix(text: &str, width: usize) -> String {
    let mut consumed = 0;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if consumed + character_width > width {
            break;
        }
        consumed += character_width;
        start = index;
    }
    text[start..].to_owned()
}

fn dropdown_widget_text(lua: &Lua, table: Table) -> String {
    let selected = table
        .get::<Option<u64>>("selected")
        .ok()
        .flatten()
        .and_then(|value| value.checked_sub(1))
        .map(|value| value as usize);
    let options = table
        .get::<Option<Table>>("options")
        .ok()
        .flatten()
        .map(|options| {
            options
                .sequence_values::<LuaValue>()
                .flatten()
                .enumerate()
                .map(|(index, value)| {
                    let text = collection_item_text(lua, value, false);
                    if selected == Some(index) {
                        format!("▼ {text}")
                    } else {
                        format!("  {text}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if options.is_empty() {
        table
            .get::<Option<String>>("label")
            .ok()
            .flatten()
            .unwrap_or_else(|| "▼".to_owned())
    } else {
        options
    }
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

fn markdown_widget_lines(lua: &Lua, value: LuaValue) -> mlua::Result<Vec<String>> {
    let (markdown, wrap_width) = match value {
        LuaValue::String(value) => (value.to_str()?.to_owned(), None),
        LuaValue::Table(value) => (
            value
                .get::<Option<LuaValue>>("text")?
                .map(|text| lua_value_text(lua, text))
                .unwrap_or_default(),
            value.get::<Option<u64>>("wrap_width")?,
        ),
        value => (lua_value_text(lua, value), None),
    };
    let wrap_width = wrap_width.map(|width| width as usize);
    if wrap_width == Some(0) {
        return Err(mlua::Error::external("wrap_width must be positive"));
    }
    let mut fence: Option<(char, usize)> = None;
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let first = line
            .chars()
            .position(|character| !character.is_whitespace())
            .unwrap_or(line.chars().count());
        let chars: Vec<char> = line.chars().collect();
        let marker = chars.get(first).copied();
        let fence_length = marker
            .filter(|marker| *marker == '`' || *marker == '~')
            .map(|marker| {
                chars[first..]
                    .iter()
                    .take_while(|character| **character == marker)
                    .count()
            })
            .filter(|length| *length >= 3);
        if let Some((active_marker, active_length)) = fence {
            let closing = marker == Some(active_marker)
                && fence_length.is_some_and(|length| length >= active_length);
            lines.extend(wrap_markdown_line(line, wrap_width));
            if closing {
                fence = None;
            }
        } else if let Some(length) = fence_length {
            lines.extend(wrap_markdown_line(line, wrap_width));
            fence = Some((marker.expect("fence marker"), length));
        } else {
            let rendered = markdown_widget_line(line);
            lines.extend(wrap_markdown_line(&rendered, wrap_width));
        }
    }
    Ok(lines)
}

fn wrap_markdown_line(line: &str, width: Option<usize>) -> Vec<String> {
    let Some(width) = width.filter(|width| *width > 0) else {
        return vec![line.to_owned()];
    };
    if UnicodeWidthStr::width(line) <= width {
        return vec![line.to_owned()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for word in line.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        let separator_width = usize::from(!current.is_empty());
        if !current.is_empty() && current_width + separator_width + word_width > width {
            result.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        vec![line.to_owned()]
    } else {
        result
    }
}

fn wrap_plain_text(text: &str, width: usize) -> String {
    text.lines()
        .flat_map(|line| wrap_markdown_line(line, Some(width)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn label_widget_text(lua: &Lua, value: &LuaValue) -> mlua::Result<String> {
    let LuaValue::Table(value) = value else {
        return Ok(lua_value_text(lua, value.clone()));
    };
    let text = value
        .get::<Option<LuaValue>>("text")?
        .map(|text| lua_value_text(lua, text))
        .unwrap_or_default();
    let badge = value
        .get::<Option<LuaValue>>("badge")?
        .map(|badge| lua_value_text(lua, badge))
        .filter(|badge| !badge.is_empty());
    let label = match badge {
        Some(badge) => format!("{text}  {badge}"),
        None => text,
    };
    let padding_left = value
        .get::<Option<LuaValue>>("padding_left")?
        .as_ref()
        .and_then(|value| lua_value_usize(Some(value)))
        .unwrap_or(0)
        .min(256);
    let padding_right = value
        .get::<Option<LuaValue>>("padding_right")?
        .as_ref()
        .and_then(|value| lua_value_usize(Some(value)))
        .unwrap_or(0)
        .min(256);
    let margin_top = value
        .get::<Option<LuaValue>>("margin_top")?
        .as_ref()
        .and_then(|value| lua_value_usize(Some(value)))
        .unwrap_or(0)
        .min(256);
    let margin_bottom = value
        .get::<Option<LuaValue>>("margin_bottom")?
        .as_ref()
        .and_then(|value| lua_value_usize(Some(value)))
        .unwrap_or(0)
        .min(256);
    let mut lines = Vec::with_capacity(margin_top + margin_bottom + 1);
    lines.extend(std::iter::repeat_n(String::new(), margin_top));
    lines.push(format!(
        "{}{label}{}",
        " ".repeat(padding_left),
        " ".repeat(padding_right)
    ));
    lines.extend(std::iter::repeat_n(String::new(), margin_bottom));
    Ok(lines.join("\n"))
}

fn title_widget_text(lua: &Lua, value: &Table) -> mlua::Result<String> {
    label_widget_text(lua, &LuaValue::Table(value.clone()))
}

fn markdown_widget_line(line: &str) -> String {
    let content = line
        .strip_prefix("### ")
        .or_else(|| line.strip_prefix("## "))
        .or_else(|| line.strip_prefix("# "))
        .unwrap_or(line);
    let mut rendered = markdown_inline_spans(content)
        .into_iter()
        .map(|(text, _style)| text)
        .collect::<String>();
    if let Some(start) = rendered.find('[') {
        if let Some(label_end) = rendered[start + 1..].find("](") {
            let label_end = start + 1 + label_end;
            if let Some(url_end) = rendered[label_end + 2..].find(')') {
                let prefix = rendered[..start].to_owned();
                let label = rendered[start + 1..label_end].to_owned();
                let suffix = rendered[label_end + 3 + url_end..].to_owned();
                rendered = format!("{prefix}{label}{suffix}");
            }
        }
    }
    rendered
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

#[cfg(test)]
fn password_widget_text(lua: &Lua, value: LuaValue) -> String {
    let (text, placeholder) = match &value {
        LuaValue::Table(table) => (
            table
                .get::<Option<String>>("text")
                .ok()
                .flatten()
                .or_else(|| table.get::<Option<String>>("value").ok().flatten())
                .unwrap_or_default(),
            table
                .get::<Option<String>>("placeholder")
                .ok()
                .flatten()
                .unwrap_or_default(),
        ),
        _ => (lua_value_text(lua, value), String::new()),
    };
    if text.is_empty() && !placeholder.is_empty() {
        format!("[{placeholder}]")
    } else {
        format!("❯ {}", "•".repeat(text.chars().count()))
    }
}

fn progress_widget_text(lua: &Lua, value: LuaValue, width: usize) -> String {
    let (ratio, label, fill, empty) = match value {
        LuaValue::Table(table) => (
            table
                .get::<Option<f64>>("value")
                .ok()
                .flatten()
                .unwrap_or(0.0),
            table.get::<Option<String>>("label").ok().flatten(),
            table
                .get::<Option<String>>("char")
                .ok()
                .flatten()
                .and_then(|text| text.chars().next())
                .unwrap_or('▄'),
            '░',
        ),
        other => (
            lua_value_text(lua, other).parse().unwrap_or(0.0),
            None,
            '▄',
            '░',
        ),
    };
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let bar = format!(
        "{}{}",
        fill.to_string().repeat(filled),
        empty.to_string().repeat(width.saturating_sub(filled))
    );
    let percent = format!("{:.0}%", ratio * 100.0);
    label.map_or_else(
        || format!("[{bar}] {percent}"),
        |label| format!("{label} [{bar}] {percent}"),
    )
}

fn image_widget_text(lua: &Lua, table: &Table) -> String {
    let alt = table
        .get::<Option<String>>("alt")
        .ok()
        .flatten()
        .filter(|value| !value.is_empty());
    let source = table
        .get::<Option<String>>("src")
        .ok()
        .flatten()
        .filter(|value| !value.is_empty());
    let data_bytes = table
        .get::<Option<mlua::String>>("data")
        .ok()
        .flatten()
        .map(|value| value.as_bytes().len());
    if let Some(length) = data_bytes {
        let label = alt.unwrap_or_else(|| "embedded image".to_owned());
        return format!("🖼 {label} ({length} bytes)");
    }
    match (alt, source) {
        (Some(alt), Some(source)) => format!("🖼 {alt} ({source})"),
        (Some(alt), None) => format!("🖼 {alt}"),
        (None, Some(source)) => format!("🖼 {source}"),
        (None, None) => lua_value_text(lua, LuaValue::Table(table.clone())),
    }
}

fn pager_widget_text(
    _lua: &Lua,
    table: &Table,
    selected_page: u64,
    pages: u64,
    limit: u64,
    total: Option<u64>,
) -> String {
    if let Some(total) = total {
        if total == 0 {
            return "No items".to_owned();
        }
        let offset = selected_page.saturating_mul(limit);
        let start = offset + 1;
        let end = (offset + limit).min(total);
        return format!("« ‹ {start}–{end} of {total} › »");
    }
    let page = table
        .get::<Option<u64>>("page")
        .ok()
        .flatten()
        .unwrap_or(selected_page + 1);
    format!("Page {page}/{pages}")
}

#[cfg(test)]
fn checkbox_widget_text(table: Table) -> String {
    let checked = table
        .get::<Option<bool>>("checked")
        .ok()
        .flatten()
        .unwrap_or(false);
    let label = table
        .get::<Option<String>>("label")
        .ok()
        .flatten()
        .or_else(|| table.get::<Option<String>>("text").ok().flatten())
        .unwrap_or_default();
    format!("[{}] {label}", if checked { 'x' } else { ' ' })
}

fn collection_item_text(lua: &Lua, value: LuaValue, table_mode: bool) -> String {
    collection_item_text_with_badge(lua, value, table_mode, true)
}

fn collection_item_text_with_badge(
    lua: &Lua,
    value: LuaValue,
    table_mode: bool,
    include_badge: bool,
) -> String {
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
    let text = item
        .get::<Option<String>>("icon")
        .ok()
        .flatten()
        .filter(|icon| !icon.is_empty())
        .map_or(text.clone(), |icon| format!("{icon} {text}"));
    let text = item
        .get::<Option<String>>("right")
        .ok()
        .flatten()
        .map_or(text.clone(), |right| format!("{text}  {right}"));
    let text = if include_badge {
        let text = item
            .get::<Option<String>>("badge_right")
            .ok()
            .flatten()
            .filter(|badge| !badge.is_empty())
            .map_or(text.clone(), |badge| format!("{text}  {badge}"));
        item.get::<Option<String>>("badge")
            .ok()
            .flatten()
            .filter(|badge| !badge.is_empty())
            .map_or(text.clone(), |badge| format!("{text}  {badge}"))
    } else {
        text
    };
    let text = item
        .get::<Option<u64>>("inset")
        .ok()
        .flatten()
        .map_or(text.clone(), |inset| {
            format!("{}{}", " ".repeat(inset.min(64) as usize), text)
        });
    item.get::<Option<Table>>("actions")
        .ok()
        .flatten()
        .map(|actions| {
            let labels = actions
                .sequence_values::<LuaValue>()
                .flatten()
                .filter_map(|action| match action {
                    LuaValue::String(value) => value.to_str().ok().map(|text| text.to_string()),
                    LuaValue::Table(action) => action
                        .get::<Option<String>>("label")
                        .ok()
                        .flatten()
                        .or_else(|| action.get::<Option<String>>("id").ok().flatten()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if labels.is_empty() {
                text.clone()
            } else {
                format!("{text}  [{}]", labels.join(" | "))
            }
        })
        .unwrap_or(text)
}

fn register_panel_action_regions(
    lua: &Lua,
    table: &Table,
    table_mode: bool,
    line: usize,
    panel_id: &str,
    regions: &Rc<RefCell<PanelActionRegions>>,
) {
    let key = if table_mode { "rows" } else { "items" };
    let Some(items) = table.get::<Option<Table>>(key).ok().flatten() else {
        return;
    };
    let mut output = regions.borrow_mut();
    let entries = output.entry(panel_id.to_owned()).or_default();
    for (index, value) in items.sequence_values::<LuaValue>().flatten().enumerate() {
        let LuaValue::Table(item) = value.clone() else {
            continue;
        };
        let Some(actions) = item.get::<Option<Table>>("actions").ok().flatten() else {
            continue;
        };
        let actions = actions
            .sequence_values::<LuaValue>()
            .flatten()
            .filter_map(|action| match action {
                LuaValue::String(label) => label
                    .to_str()
                    .ok()
                    .map(|label| (label.to_owned(), label.to_owned())),
                LuaValue::Table(action) => {
                    let command = action
                        .get::<Option<String>>("command")
                        .ok()
                        .flatten()
                        .or_else(|| action.get::<Option<String>>("id").ok().flatten());
                    let label = action
                        .get::<Option<String>>("label")
                        .ok()
                        .flatten()
                        .or_else(|| command.clone())
                        .or_else(|| {
                            action
                                .get::<Option<bool>>("menu")
                                .ok()
                                .flatten()
                                .filter(|menu| *menu)
                                .map(|_| "⋮".to_owned())
                        })?;
                    Some((label, command.unwrap_or_default()))
                }
                _ => None,
            })
            .filter(|(_, command)| !command.is_empty())
            .collect::<Vec<_>>();
        if actions.is_empty() {
            continue;
        }
        let text =
            collection_item_text_with_badge(lua, LuaValue::Table(item.clone()), table_mode, true);
        let labels = actions
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        let block = format!("[{labels}]");
        let block_start = 2 + UnicodeWidthStr::width(text.as_str())
            .saturating_sub(UnicodeWidthStr::width(block.as_str()));
        let mut cursor = block_start + 1;
        for (label, command) in actions {
            let end = cursor + UnicodeWidthStr::width(label.as_str());
            entries.push(PanelActionRegion {
                line,
                start: cursor,
                end,
                command,
                row: index + 1,
            });
            cursor = end + 3;
        }
    }
    if table_mode {
        let Some(columns) = table.get::<Option<Table>>("columns").ok().flatten() else {
            return;
        };
        let rendered = collection_widget_text(lua, LuaValue::Table(table.clone()), true);
        for (column_index, column) in columns.sequence_values::<Table>().flatten().enumerate() {
            let Some(actions) = column.get::<Option<Table>>("actions").ok().flatten() else {
                continue;
            };
            let actions = actions
                .sequence_values::<LuaValue>()
                .flatten()
                .filter_map(|action| match action {
                    LuaValue::String(label) => label
                        .to_str()
                        .ok()
                        .map(|label| (label.to_owned(), label.to_owned())),
                    LuaValue::Table(action) => {
                        let command = action
                            .get::<Option<String>>("command")
                            .ok()
                            .flatten()
                            .or_else(|| action.get::<Option<String>>("id").ok().flatten());
                        let label = action
                            .get::<Option<String>>("label")
                            .ok()
                            .flatten()
                            .or_else(|| command.clone())
                            .or_else(|| {
                                action
                                    .get::<Option<bool>>("menu")
                                    .ok()
                                    .flatten()
                                    .filter(|menu| *menu)
                                    .map(|_| "⋮".to_owned())
                            })?;
                        Some((label, command.unwrap_or_default()))
                    }
                    _ => None,
                })
                .filter(|(_, command)| !command.is_empty())
                .collect::<Vec<_>>();
            if actions.is_empty() {
                continue;
            }
            let labels = actions
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            for (row, row_text) in rendered.lines().skip(1).take(items.raw_len()).enumerate() {
                let cells = table_row_cells(
                    lua,
                    &items.get::<LuaValue>(row + 1).unwrap_or(LuaValue::Nil),
                );
                if column_index > cells.len() {
                    continue;
                }
                let Some(byte_start) = row_text.find(&labels) else {
                    continue;
                };
                let start = UnicodeWidthStr::width(&row_text[..byte_start]) + 1;
                let mut cursor = start;
                for (label, command) in &actions {
                    let end = cursor + UnicodeWidthStr::width(label.as_str());
                    entries.push(PanelActionRegion {
                        line: line + row + 1,
                        start: cursor,
                        end,
                        command: command.clone(),
                        row: row + 1,
                    });
                    cursor = end + 3;
                }
            }
        }
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

fn lua_error(error: mlua::Error) -> crate::error::AppError {
    plugin_error(error.to_string())
}

fn configure_lua_sandbox(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in [
        "dofile",
        "loadfile",
        "load",
        "loadstring",
        "getfenv",
        "setfenv",
        "rawget",
        "rawset",
        "print",
        "debug",
        "io",
    ] {
        globals.set(name, LuaValue::Nil)?;
    }

    let started = Instant::now();
    let safe_os = lua.create_table()?;
    safe_os.set(
        "time",
        lua.create_function(|_, ()| {
            Ok(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(mlua::Error::external)?
                .as_secs())
        })?,
    )?;
    safe_os.set(
        "clock",
        lua.create_function(move |_, ()| Ok(started.elapsed().as_secs_f64()))?,
    )?;
    safe_os.set(
        "date",
        lua.create_function(|_, format: Option<String>| {
            let format = format.unwrap_or_else(|| "%c".to_owned());
            Ok(safe_os_date(&format))
        })?,
    )?;
    globals.set("os", safe_os)?;

    let package = globals.get::<Table>("package")?;
    package.set("path", "")?;
    package.set("cpath", "")?;
    package.set("loadlib", LuaValue::Nil)?;
    package.set("searchpath", LuaValue::Nil)?;
    let searchers = package.get::<Table>("searchers")?;
    let preload_searcher = searchers.get::<LuaValue>(1)?;
    let safe_searchers = lua.create_table()?;
    safe_searchers.set(1, preload_searcher)?;
    package.set("searchers", safe_searchers)?;
    Ok(())
}

fn safe_os_date(format: &str) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_date_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let hour_12 = match hour % 12 {
        0 => 12,
        value => value,
    };
    let meridiem = if hour < 12 { "AM" } else { "PM" };
    let weekday = ((days + 4).rem_euclid(7)) as usize;
    let weekdays = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let short_weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let months = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let short_months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month_index = (month - 1) as usize;
    let mut output = String::new();
    let mut chars = format.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(specifier) = chars.next() else {
            output.push('%');
            break;
        };
        let rendered = match specifier {
            '%' => "%".to_owned(),
            'Y' => format!("{year:04}"),
            'y' => format!("{:02}", year.rem_euclid(100)),
            'm' => format!("{month:02}"),
            'd' => format!("{day:02}"),
            'H' => format!("{hour:02}"),
            'I' => format!("{hour_12:02}"),
            'M' => format!("{minute:02}"),
            'S' => format!("{second:02}"),
            'p' => meridiem.to_owned(),
            'Z' => "UTC".to_owned(),
            'A' => weekdays[weekday].to_owned(),
            'a' => short_weekdays[weekday].to_owned(),
            'B' => months[month_index].to_owned(),
            'b' => short_months[month_index].to_owned(),
            'c' => format!(
                "{} {} {:02} {:02}:{:02}:{:02} {:04}",
                short_weekdays[weekday], short_months[month_index], day, hour, minute, second, year
            ),
            _ => String::new(),
        };
        output.push_str(&rendered);
    }
    output
}

fn civil_date_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month + 2) / 5 + 1;
    let year = year + if month < 10 { 0 } else { 1 };
    let month = month + if month < 10 { 3 } else { -9 };
    (year, month, day)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        hex_encode(&bytes[0..4]),
        hex_encode(&bytes[4..6]),
        hex_encode(&bytes[6..8]),
        hex_encode(&bytes[8..10]),
        hex_encode(&bytes[10..16])
    )
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let value = (first << 16) | (second << 8) | third;
        output.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        output.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn base64_decode(value: &str) -> Result<Vec<u8>, String> {
    let mut normalized = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if normalized.len() % 4 == 2 {
        normalized.extend_from_slice(b"==");
    } else if normalized.len() % 4 == 3 {
        normalized.push(b'=');
    }
    let bytes = normalized.as_slice();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 input length must be a multiple of 4".to_owned());
    }
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let first = base64_value(chunk[0])?;
        let second = base64_value(chunk[1])?;
        let third = if chunk[2] == b'=' {
            0
        } else {
            base64_value(chunk[2])?
        };
        let fourth = if chunk[3] == b'=' {
            0
        } else {
            base64_value(chunk[3])?
        };
        if !last && (chunk[2] == b'=' || chunk[3] == b'=') {
            return Err("base64 padding is only allowed in the final quartet".to_owned());
        }
        if chunk[2] == b'=' && chunk[3] != b'=' {
            return Err("invalid base64 padding".to_owned());
        }
        let combined = (first << 18) | (second << 12) | (third << 6) | fourth;
        output.push((combined >> 16) as u8);
        if chunk[2] != b'=' {
            output.push((combined >> 8) as u8);
        }
        if chunk[3] != b'=' {
            output.push(combined as u8);
        }
    }
    Ok(output)
}

fn base64_value(value: u8) -> Result<u32, String> {
    match value {
        b'A'..=b'Z' => Ok(u32::from(value - b'A')),
        b'a'..=b'z' => Ok(u32::from(value - b'a' + 26)),
        b'0'..=b'9' => Ok(u32::from(value - b'0' + 52)),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("invalid base64 character".to_owned()),
    }
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
            let magic = item
                .get::<Option<String>>("magic")?
                .filter(|magic| !magic.is_empty())
                .map(String::into_bytes);
            Ok(PluginFileHandler {
                id,
                title,
                extensions,
                magic,
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

fn rpc_endpoint_allowed(endpoint: &str, allowed: &[String], allow_all: bool) -> bool {
    if allow_all {
        return true;
    }
    let endpoint = endpoint.trim();
    let normalized_endpoint = endpoint.strip_prefix("tcp://").unwrap_or(endpoint);
    allowed.iter().any(|candidate| {
        let candidate = candidate.trim();
        if let Some(candidate) = candidate.strip_prefix("unix:") {
            endpoint.strip_prefix("unix:") == Some(candidate)
        } else {
            candidate.strip_prefix("tcp://").unwrap_or(candidate) == normalized_endpoint
        }
    })
}

fn write_rpc_message(stream: &mut RpcTransport, message: &JsonValue) -> Result<(), String> {
    let encoded = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    if encoded.len() > 32 * 1024 * 1024 {
        return Err("RPC message exceeds 32 MiB".to_owned());
    }
    let header = format!("Content-Length: {}\r\n\r\n", encoded.len());
    stream
        .write_all(header.as_bytes())
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&encoded)
        .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

fn read_rpc_message(reader: &mut BufReader<RpcTransport>) -> Result<Option<JsonValue>, String> {
    let mut first = String::new();
    let read = reader
        .read_line(&mut first)
        .map_err(|error| error.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    if first.trim_start().starts_with('{') {
        return serde_json::from_str(first.trim())
            .map(Some)
            .map_err(|error| format!("invalid RPC JSON: {error}"));
    }

    let mut content_length = None;
    let mut header = first;
    let mut header_bytes = 0usize;
    loop {
        header_bytes = header_bytes.saturating_add(header.len());
        if header_bytes > 8 * 1024 {
            return Err("RPC headers exceed 8 KiB".to_owned());
        }
        let trimmed = header.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "invalid RPC Content-Length".to_owned())?,
                );
            }
        }
        header.clear();
        reader
            .read_line(&mut header)
            .map_err(|error| error.to_string())?;
    }
    let length = content_length.ok_or_else(|| "RPC frame missing Content-Length".to_owned())?;
    if length > 32 * 1024 * 1024 {
        return Err("RPC message exceeds 32 MiB".to_owned());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("invalid RPC JSON: {error}"))
}

fn spawn_rpc_reader(connection_id: u64, stream: RpcTransport, events: Sender<RpcEvent>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let message = match read_rpc_message(&mut reader) {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => {
                    let _ = events.send(RpcEvent {
                        connection_id,
                        kind: RpcEventKind::Closed,
                    });
                    break;
                }
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

fn apply_editor_request_to_snapshot(
    snapshot: &mut PluginEditorSnapshot,
    request: &PluginEditorRequest,
) {
    if snapshot.lines.is_empty() {
        snapshot.lines.push(String::new());
    }
    match request {
        PluginEditorRequest::SetLine { line, text } => {
            if let Some(current) = snapshot.lines.get_mut(*line) {
                *current = text.clone();
            }
        }
        PluginEditorRequest::SetCursor { line, col } => {
            let line = (*line).min(snapshot.lines.len().saturating_sub(1));
            snapshot.cursor_line = line;
            snapshot.cursor_col = (*col).min(snapshot.lines[line].chars().count());
        }
        PluginEditorRequest::Insert { line, col, text } => {
            let line = (*line).min(snapshot.lines.len().saturating_sub(1));
            let col = (*col).min(snapshot.lines[line].chars().count());
            replace_snapshot_range(snapshot, line, col, line, col, text);
        }
        PluginEditorRequest::Replace {
            start_line,
            start_col,
            end_line,
            end_col,
            text,
        } => {
            let start_line = (*start_line).min(snapshot.lines.len().saturating_sub(1));
            let end_line = (*end_line).min(snapshot.lines.len().saturating_sub(1));
            let start_col = (*start_col).min(snapshot.lines[start_line].chars().count());
            let end_col = (*end_col).min(snapshot.lines[end_line].chars().count());
            replace_snapshot_range(snapshot, start_line, start_col, end_line, end_col, text);
        }
        _ => {}
    }
}

fn replace_snapshot_range(
    snapshot: &mut PluginEditorSnapshot,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
    text: &str,
) {
    let offset = |line: usize, column: usize| {
        snapshot
            .lines
            .iter()
            .take(line)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>()
            .saturating_add(column)
    };
    let source = snapshot.lines.join("\n");
    let mut characters: Vec<char> = source.chars().collect();
    let start = offset(start_line, start_col).min(characters.len());
    let end = offset(end_line, end_col).min(characters.len()).max(start);
    let replacement: Vec<char> = text.chars().collect();
    characters.splice(start..end, replacement.iter().copied());
    snapshot.lines = characters
        .iter()
        .collect::<String>()
        .split('\n')
        .map(str::to_owned)
        .collect();
    let inserted_lines: Vec<&str> = text.split('\n').collect();
    snapshot.cursor_line = start_line + inserted_lines.len().saturating_sub(1);
    snapshot.cursor_col = if inserted_lines.len() > 1 {
        inserted_lines.last().map_or(0, |line| line.chars().count())
    } else {
        start_col.saturating_add(
            inserted_lines
                .first()
                .map_or(0, |line| line.chars().count()),
        )
    };
    snapshot.cursor_line = snapshot
        .cursor_line
        .min(snapshot.lines.len().saturating_sub(1));
    snapshot.cursor_col = snapshot
        .cursor_col
        .min(snapshot.lines[snapshot.cursor_line].chars().count());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_editor_writes_are_visible_to_later_plugin_reads() {
        let mut snapshot = PluginEditorSnapshot {
            lines: vec!["a你b".to_owned(), "tail".to_owned()],
            ..PluginEditorSnapshot::default()
        };
        let first = PluginEditorRequest::Replace {
            start_line: 0,
            start_col: 3,
            end_line: 1,
            end_col: 0,
            text: " ".to_owned(),
        };
        apply_editor_request_to_snapshot(&mut snapshot, &first);
        assert_eq!(snapshot.lines, ["a你b tail"]);

        let next = PluginEditorRequest::Replace {
            start_line: 0,
            start_col: 4,
            end_line: 0,
            end_col: 8,
            text: "done".to_owned(),
        };
        apply_editor_request_to_snapshot(&mut snapshot, &next);
        assert_eq!(snapshot.lines, ["a你b done"]);
    }

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
    fn plugin_argument_accepts_entry_file_and_package_directory() {
        let root = std::env::temp_dir().join(format!("mtide-plugin-entry-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("plugin directory");
        std::fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"entry-file","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        std::fs::write(root.join("init.lua"), "ttt.register({})").expect("entry file");

        let from_directory = PluginPackage::load_argument(&root).expect("directory argument");
        let from_file =
            PluginPackage::load_argument(root.join("init.lua")).expect("entry file argument");
        assert_eq!(from_directory.entry_path(), from_file.entry_path());
        assert_eq!(from_file.manifest.name, "entry-file");

        std::fs::remove_dir_all(root).expect("cleanup plugin fixture");
    }

    #[test]
    fn plugin_commands_can_queue_editor_undo_and_redo() {
        let root =
            std::env::temp_dir().join(format!("mtide-plugin-undo-command-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("plugin directory");
        std::fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"undo-command","entry":"init.lua","api":2,"permissions":{"commands":true,"editor.write":true}}"#,
        )
        .expect("manifest");
        std::fs::write(
            root.join("init.lua"),
            "local ttt=require('ttt'); assert(ttt.exec_command('editor.undo')); assert(ttt.exec_command('editor.redo'))",
        )
        .expect("entry file");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.drain_editor_requests(),
            vec![PluginEditorRequest::Undo, PluginEditorRequest::Redo]
        );
        std::fs::remove_dir_all(root).expect("cleanup plugin fixture");
    }

    #[test]
    fn plugin_settings_reload_command_requests_host_theme_reload() {
        let root = std::env::temp_dir().join(format!(
            "mtide-plugin-settings-reload-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("plugin directory");
        std::fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"settings-reload","entry":"init.lua","api":2,"permissions":{"commands":true}}"#,
        )
        .expect("manifest");
        std::fs::write(
            root.join("init.lua"),
            "local ttt=require('ttt'); assert(ttt.exec_command('settings.reload'))",
        )
        .expect("entry file");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::ReloadTheme]
        );
        std::fs::remove_dir_all(root).expect("cleanup plugin fixture");
    }

    #[test]
    fn dropdown_widget_renders_selected_option() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        let options = lua.create_table().expect("options");
        options.set(1, "one").expect("option");
        options.set(2, "two").expect("option");
        table.set("options", options).expect("options");
        table.set("selected", 2_u64).expect("selected");
        assert_eq!(dropdown_widget_text(&lua, table), "  one\n▼ two");
    }

    #[test]
    fn checkbox_widget_renders_checked_state_and_label() {
        let lua = Lua::new();
        let checked = lua.create_table().expect("table");
        checked.set("checked", true).expect("checked");
        checked.set("label", "Enabled").expect("label");
        assert_eq!(checkbox_widget_text(checked), "[x] Enabled");
        let unchecked = lua.create_table().expect("table");
        unchecked.set("label", "Disabled").expect("label");
        assert_eq!(checkbox_widget_text(unchecked), "[ ] Disabled");
    }

    #[test]
    fn progress_widget_clamps_ratio_and_keeps_label() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        table.set("value", 1.5_f64).expect("value");
        table.set("label", "Build").expect("label");
        table.set("char", "#").expect("char");
        let text = progress_widget_text(&lua, LuaValue::Table(table), 20);
        assert!(text.starts_with("Build [####################] 100%"));

        let empty = lua.create_table().expect("empty");
        empty.set("value", -1.0_f64).expect("value");
        assert_eq!(
            progress_widget_text(&lua, LuaValue::Table(empty), 20),
            "[░░░░░░░░░░░░░░░░░░░░] 0%"
        );
    }

    #[test]
    fn progress_widget_uses_panel_width() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        table.set("value", 0.5_f64).expect("value");
        assert_eq!(
            progress_widget_text(&lua, LuaValue::Table(table), 10),
            "[▄▄▄▄▄░░░░░] 50%"
        );
    }

    #[test]
    fn password_widget_masks_value_and_keeps_placeholder() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        table.set("text", "secret").expect("text");
        assert_eq!(
            password_widget_text(&lua, LuaValue::Table(table)),
            "❯ ••••••"
        );

        let empty = lua.create_table().expect("empty table");
        empty.set("placeholder", "Password").expect("placeholder");
        assert_eq!(
            password_widget_text(&lua, LuaValue::Table(empty)),
            "[Password]"
        );
    }

    #[test]
    fn table_widget_applies_unicode_safe_horizontal_scroll() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        let rows = lua.create_table().expect("rows");
        let row = lua.create_table().expect("row");
        row.set("label", "甲乙丙丁").expect("label");
        rows.set(1, row).expect("row");
        table.set("rows", rows).expect("rows");
        table.set("scroll_x", 1_u64).expect("scroll");
        table.set("width", 2_u64).expect("width");
        assert_eq!(
            collection_widget_text(&lua, LuaValue::Table(table), true),
            "  乙丙"
        );
    }

    #[test]
    fn collection_rows_render_action_labels() {
        let lua = Lua::new();
        let row = lua.create_table().expect("row");
        row.set("label", "item").expect("label");
        let actions = lua.create_table().expect("actions");
        let action = lua.create_table().expect("action");
        action.set("id", "open").expect("id");
        action.set("label", "Open").expect("label");
        actions.set(1, action).expect("action");
        row.set("actions", actions).expect("actions");
        assert_eq!(
            collection_item_text(&lua, LuaValue::Table(row), false),
            "item  [Open]"
        );
        let string_row = lua.create_table().expect("string row");
        string_row.set("label", "item").expect("label");
        let string_actions = lua.create_table().expect("string actions");
        string_actions.set(1, "Delete").expect("action");
        string_row.set("actions", string_actions).expect("actions");
        assert_eq!(
            collection_item_text(&lua, LuaValue::Table(string_row), false),
            "item  [Delete]"
        );
    }

    #[test]
    fn collection_rows_render_icon_and_badge() {
        let lua = Lua::new();
        let row = lua.create_table().expect("row");
        row.set("label", "server").expect("label");
        row.set("icon", "●").expect("icon");
        row.set("badge", "running").expect("badge");
        assert_eq!(
            collection_item_text(&lua, LuaValue::Table(row), false),
            "● server  running"
        );
    }

    #[test]
    fn collection_rows_render_badge_right_and_inset() {
        let lua = Lua::new();
        let row = lua.create_table().expect("row");
        row.set("label", "child").expect("label");
        row.set("badge_right", "new").expect("badge");
        row.set("inset", 2_u64).expect("inset");
        assert_eq!(
            collection_item_text(&lua, LuaValue::Table(row), false),
            "  child  new"
        );
    }

    #[test]
    fn list_widget_aligns_badges_to_the_right_edge() {
        let lua = Lua::new();
        let list = lua.create_table().expect("list");
        let items = lua.create_table().expect("items");
        let item = lua.create_table().expect("item");
        item.set("label", "server").expect("label");
        item.set("badge", "up").expect("badge");
        items.set(1, item).expect("item");
        list.set("items", items).expect("items");
        list.set("badge_right", true).expect("badge_right");
        assert_eq!(
            collection_widget_text_with_width(&lua, LuaValue::Table(list), false, Some(16)),
            "  server      up"
        );
    }

    #[test]
    fn tree_widget_renders_expanded_children_and_selection() {
        let lua = Lua::new();
        let tree = lua.create_table().expect("tree");
        let items = lua.create_table().expect("items");
        let root = lua.create_table().expect("root");
        root.set("label", "src").expect("label");
        root.set("expanded", true).expect("expanded");
        let children = lua.create_table().expect("children");
        let child = lua.create_table().expect("child");
        child.set("label", "main.rs").expect("label");
        children.set(1, child).expect("child");
        root.set("children", children).expect("children");
        items.set(1, root).expect("root");
        tree.set("items", items).expect("items");
        tree.set("selected", 2_u64).expect("selected");
        assert_eq!(
            tree_widget_text_with_width(&lua, &tree, None),
            "▾ src\n  ▶ main.rs"
        );
    }

    #[test]
    fn tree_widget_honors_configured_indent() {
        let lua = Lua::new();
        let tree = lua.create_table().expect("tree");
        let items = lua.create_table().expect("items");
        let root = lua.create_table().expect("root");
        root.set("label", "root").expect("label");
        root.set("expanded", true).expect("expanded");
        let children = lua.create_table().expect("children");
        let child = lua.create_table().expect("child");
        child.set("label", "child").expect("label");
        children.set(1, child).expect("child");
        root.set("children", children).expect("children");
        items.set(1, root).expect("root");
        tree.set("items", items).expect("items");
        tree.set("indent", 4_u64).expect("indent");
        assert_eq!(
            tree_widget_text_with_width(&lua, &tree, None),
            "▾ root\n      child"
        );
    }

    #[test]
    fn tree_widget_truncates_labels_from_left_using_unicode_width() {
        let lua = Lua::new();
        let tree = lua.create_table().expect("tree");
        let items = lua.create_table().expect("items");
        let item = lua.create_table().expect("item");
        item.set("label", "目录/配置/设置.toml").expect("label");
        items.set(1, item).expect("item");
        tree.set("items", items).expect("items");
        tree.set("truncate_left", true).expect("truncate");
        assert_eq!(
            tree_widget_text_with_width(&lua, &tree, Some(12)),
            "  …设置.toml"
        );
    }

    #[test]
    fn editor_read_api_exposes_snapshot_with_one_based_positions() {
        let root = std::env::temp_dir().join(format!("mtide-editor-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"editor-api","entry":"init.lua","api":2,"permissions":{"editor.read":true,"editor.write":true,"editor.diagnostics":true,"editor.decorations":true,"editor.bookmarks":true,"screen.read":true,"terminal.read":true,"fs.write":true}}"#,
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
  assert(ttt.screenshot("screen.txt") == true)
  assert(ttt.debug("debug.json") == true)
  assert(editor.byte_to_col("你a", 4) == 2)
  assert(editor.col_to_byte("你a", 2) == 4)
  assert(editor.viewport().top_line == 1)
  assert(editor.viewport().bottom_line == 2 and editor.viewport().height == 2)
  assert(editor.scroll_to(2) == true)
  assert(editor.scroll_by(1) == true)
  assert(editor.set_search("one") == true)
  assert(editor.set_search("o.e", true) == true)
  assert(editor.clear_search() == true)
  local cursor = editor.cursor()
  assert(cursor.line == 2 and cursor.col == 3)
  assert(editor.current_word() == "two")
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
  assert(diagnostics.publish("/tmp/shared.rs", {{line = 1, col = 1, severity = "error", message = "shared diagnostic"}}) == true)
  assert(decorations.set("/tmp/shared.rs", {{line = 1, style = "info"}}) == true)
  assert(diagnostics.clear("/tmp/shared.rs") == true)
  assert(diagnostics.publish("/tmp/kept.rs", {{line = 1, col = 1, severity = "warning", message = "kept diagnostic"}}) == true)
  assert(decorations.set("/tmp/kept.rs", {{line = 1, style = "hint"}}) == true)
  assert(decorations.clear("/tmp/kept.rs") == true)
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
            viewport_top_line: 0,
            viewport_left_col: 0,
            viewport_height: 2,
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
        assert!(root.join("screen.txt").is_file());
        assert!(root.join("debug.json").is_file());
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
        assert_eq!(runtime.diagnostic_severity("/tmp/decor.rs", 2), None);
        assert_eq!(
            runtime.decoration_style("/tmp/decor.rs", 2).as_deref(),
            Some("hint")
        );
        assert_eq!(
            runtime.diagnostic_message("/tmp/decor.rs", 2).as_deref(),
            None
        );
        assert_eq!(
            runtime.diagnostic_message("/tmp/demo.rs", 1).as_deref(),
            Some("bad line")
        );
        assert_eq!(runtime.diagnostic_severity("/tmp/shared.rs", 0), None);
        assert_eq!(
            runtime.decoration_style("/tmp/shared.rs", 0).as_deref(),
            Some("info")
        );
        assert_eq!(runtime.diagnostic_severity("/tmp/kept.rs", 0), Some(2));
        assert_eq!(runtime.decoration_style("/tmp/kept.rs", 0), None);
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
    fn echo_api_takes_over_and_clears_status_text() {
        let root = std::env::temp_dir().join(format!("mtide-echo-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"echo-api","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
ttt.set_echo("building")
ttt.notify("failed", "error")
assert(type(ttt.clear_echo) == "function")
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(runtime.echo_text(), "building");
        assert_eq!(
            runtime.notification(),
            Some(("failed".to_owned(), "error".to_owned()))
        );
        runtime
            ._lua
            .load("require(\"ttt\").clear_echo()")
            .exec()
            .expect("clear echo");
        assert!(runtime.echo_text().is_empty());
        let _ = fs::remove_dir_all(root);
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
    fn plugin_registry_rejects_insecure_endpoints_before_network_access() {
        assert!(fetch_plugin_registry("http://registry.example/plugins.json").is_err());
        assert!(fetch_plugin_registry("https://registry.example/plugins.json\n").is_err());
        assert!(
            fetch_plugin_registry("https://user:secret@registry.example/plugins.json").is_err()
        );
        assert!(fetch_plugin_registry("https://registry.example:8443/plugins.json").is_err());
        for host in [
            "localhost",
            "127.0.0.1",
            "10.0.0.1",
            "[::1]",
            "169.254.169.254",
        ] {
            assert!(fetch_plugin_registry(&format!("https://{host}/plugins.json")).is_err());
        }
    }

    #[test]
    fn plugin_registry_entry_requires_secure_download_url() {
        let raw = r#"[{"name":"demo","version":"1","url":"http://example.com/demo.zip"}]"#;
        let entries: Vec<PluginRegistryEntry> = serde_json::from_str(raw).expect("index");
        assert!(!entries[0].url.starts_with("https://"));
    }

    #[test]
    fn plugin_registry_selection_requires_unique_name_and_version() {
        let entries = vec![
            PluginRegistryEntry {
                name: "demo".into(),
                version: "1.0.0".into(),
                url: "https://example.com/demo.zip".into(),
                sha256: None,
            },
            PluginRegistryEntry {
                name: "demo".into(),
                version: "2.0.0".into(),
                url: "https://example.com/demo-2.zip".into(),
                sha256: None,
            },
        ];
        assert_eq!(
            select_plugin_registry_entry(&entries, "demo", Some("2.0.0"))
                .expect("version")
                .url,
            "https://example.com/demo-2.zip"
        );
        assert_eq!(
            select_plugin_registry_entry(&entries, "demo", None)
                .expect("latest")
                .version,
            "2.0.0"
        );
        assert!(select_plugin_registry_entry(&entries, "missing", None).is_err());
    }

    #[test]
    fn plugin_registry_download_rejects_invalid_entry_before_network_access() {
        let entry = PluginRegistryEntry {
            name: "demo".into(),
            version: "1.0.0".into(),
            url: "http://example.com/demo.zip".into(),
            sha256: None,
        };
        assert!(download_plugin_registry_entry(&entry).is_err());
    }

    #[test]
    fn plugin_registry_download_rejects_url_credentials_and_nonstandard_ports() {
        for url in [
            "https://user:secret@example.com/demo.zip",
            "https://example.com:8443/demo.zip",
        ] {
            let entry = PluginRegistryEntry {
                name: "demo".into(),
                version: "1.0.0".into(),
                url: url.into(),
                sha256: None,
            };
            assert!(download_plugin_registry_entry(&entry).is_err(), "{url}");
        }
    }

    #[test]
    fn plugin_package_sha256_is_verified() {
        assert!(plugin_package_hash_matches(
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        assert!(!plugin_package_hash_matches(b"plugin", "bad"));
    }

    #[test]
    fn plugin_archive_member_paths_are_restricted() {
        assert!(safe_plugin_archive_member("plugin.ttt.json"));
        assert!(safe_plugin_archive_member("nested/init.lua"));
        assert!(!safe_plugin_archive_member("../escape.lua"));
        assert!(!safe_plugin_archive_member("/absolute.lua"));
        assert!(!safe_plugin_archive_member("nested\\escape.lua"));
    }

    #[test]
    fn plugin_archive_install_rejects_empty_and_existing_destinations() {
        let root = std::env::temp_dir().join(format!("mtide-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        let destination = root.join("plugin");
        fs::create_dir_all(&destination).expect("destination");
        assert!(install_plugin_archive(b"", &destination).is_err());
        assert!(install_plugin_archive(b"not zip", &destination).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_archive_update_rolls_back_when_new_archive_is_invalid() {
        let root = std::env::temp_dir().join(format!("mtide-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let destination = root.join("plugin");
        fs::create_dir_all(&destination).expect("destination");
        fs::write(destination.join("keep.txt"), "old").expect("old plugin");
        assert!(update_plugin_archive(b"not zip", &destination).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("keep.txt")).expect("restored"),
            "old"
        );
        let _ = fs::remove_dir_all(root);
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
            &hosts,
            false,
        ));
        assert!(network_target_allowed(
            "http://127.0.0.1:4250/",
            &hosts,
            false
        ));
        assert!(!network_target_allowed(
            "http://127.0.0.1:444/data",
            &hosts,
            false
        ));
        assert!(!network_target_allowed(
            "https://other.example.com/",
            &hosts,
            false,
        ));
        assert!(!network_target_allowed(
            "https://anything.example/",
            &[],
            false
        ));
        assert!(network_target_allowed(
            "https://anything.example/",
            &[],
            true
        ));
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
assert(ttt.open_dialog({title = "Review", width = 40, height = 8, render = function(panel) panel:label("dialog body"); panel:password({text = "old", on_change = function(text) ttt.log("input:" .. text) end}); panel:checkbox({checked = false, label = "Remember", on_change = function(value) ttt.log("checked:" .. tostring(value)) end}); panel:button({label = "Submit", on_click = function() ttt.log("clicked") end}); panel:button({label = "Cancel", on_click = function() ttt.log("cancelled") end}) end, on_close = function() ttt.log("dialog-closed") end}) == true)
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
            Some("dialog body\n▶❯ •••\n[ ] Remember\n[ Submit ]\n[ Cancel ]".into())
        );
        assert!(runtime.dialog_input(1, "new".into()).expect("input"));
        assert!(runtime
            .dialog_input_key(1, "Left", None, "")
            .expect("move dialog cursor"));
        assert!(runtime
            .dialog_input_key(1, "Char", Some('x'), "")
            .expect("insert dialog text"));
        assert!(runtime.dialog_focus_next(1));
        assert!(runtime.dialog_activate(1).expect("checkbox"));
        assert!(runtime
            .render_panel("__dialog:1")
            .expect("rerender")
            .expect("dialog")
            .contains("[x] Remember"));
        assert!(!runtime.dialog_append(1, 'x').expect("button focus"));
        assert!(runtime.dialog_activate(1).expect("button"));
        assert!(runtime.dialog_focus_next(1));
        assert!(runtime.dialog_activate(1).expect("second button"));
        assert!(runtime.dialog_focus_button(1));
        assert!(runtime.dialog_activate(1).expect("button again"));
        assert!(runtime.logs().iter().any(|line| line == "[info] input:new"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] input:nexw"));
        assert!(runtime.logs().iter().any(|line| line == "[info] clicked"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] checked:true"));
        assert!(runtime.close_custom_dialog(1).expect("close"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] dialog-closed"));
        assert!(!runtime
            .dialog_input(1, "after-close".into())
            .expect("closed input"));
    }

    #[test]
    fn custom_dialog_input_submit_callback_receives_current_text() {
        let root = std::env::temp_dir().join(format!("mtide-dialog-submit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"dialog-submit","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.open_dialog({title = "Submit", render = function(panel)
  panel:input({text = "initial", clear_on_submit = true, on_submit = function(text) ttt.log("submitted:" .. text) end})
end})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("__dialog:1").expect("render"),
            Some("❯ initial".into())
        );
        assert!(runtime.dialog_input(1, "updated".into()).expect("input"));
        assert!(runtime.dialog_activate(1).expect("submit"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] submitted:updated"));
        assert_eq!(
            runtime.render_panel("__dialog:1").expect("cleared render"),
            Some("❯ ".into())
        );
        assert!(runtime.close_custom_dialog(1).expect("close"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn custom_dialog_dropdown_has_focus_and_change_callback() {
        let root =
            std::env::temp_dir().join(format!("mtide-dropdown-dialog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"dropdown-dialog","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"
local ttt = require("ttt")
ttt.open_dialog({title = "Pick", render = function(panel)
  panel:input({text = "query"})
  panel:dropdown({options = {"one", "two"}, selected = 1, on_change = function(value) ttt.log("selected:" .. value) end})
  panel:checkbox({label = "Remember", on_change = function(value) ttt.log("remember:" .. tostring(value)) end})
  panel:button({label = "Done", on_click = function() ttt.log("done") end})
end})
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.drain_panel_requests();
        assert!(runtime
            .render_panel("__dialog:1")
            .expect("render")
            .is_some());
        assert!(runtime.dialog_focus_next(1));
        assert!(runtime.dialog_dropdown_move(1, 1).expect("move"));
        assert!(runtime.dialog_focus_next(1));
        assert!(runtime.dialog_activate(1).expect("checkbox"));
        assert!(runtime.dialog_focus_next(1));
        assert!(runtime.dialog_activate(1).expect("button"));
        assert!(runtime.dialog_focus_at_line(1, 1));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] selected:2"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn raw_panel_coordinates_and_clear_preserve_cell_positions() {
        let root = std::env::temp_dir().join(format!("mtide-raw-panel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"raw-panel","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "raw", render = function(panel)
  local size = panel:size()
  assert(size.width == 42 and size.height == 7)
  panel:text(2, 0, "xy")
  panel:cell(0, 1, "z")
  panel:clear(1, 0, 1, 1)
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.set_panel_size(42, 7);
        assert_eq!(
            runtime.render_panel("raw").expect("render"),
            Some("  xy\nz".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn editor_diff_api_reports_hunks_and_queues_navigation() {
        let root =
            std::env::temp_dir().join(format!("mtide-editor-diff-api-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"editor-diff-api","entry":"init.lua","api":2,"permissions":{"editor.read":true,"editor.write":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local editor = require("ttt.editor")
ttt.on("probe", function()
  local diff = editor.diff()
  assert(diff ~= nil and #diff.hunks == 2)
  assert(diff.hunks[1].kind == "modified")
  assert(diff.hunks[2].kind == "added")
  assert(diff.current_hunk == 1)
  assert(editor.go_to_diff_hunk(2) == true)
end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.set_diff_snapshot(Some(PluginDiffSnapshot {
            hunks: vec![
                PluginDiffHunk {
                    file_path: "/tmp/main.rs".to_owned(),
                    kind: "modified".to_owned(),
                    line: 0,
                },
                PluginDiffHunk {
                    file_path: "/tmp/main.rs".to_owned(),
                    kind: "added".to_owned(),
                    line: 2,
                },
            ],
            current_hunk: 1,
        }));
        assert!(runtime.emit("probe").expect("probe"));
        assert_eq!(
            runtime.drain_editor_requests(),
            vec![PluginEditorRequest::GoToDiffHunk { index: 1 }]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn install_hook_runs_once_and_is_not_triggered_by_loading() {
        let root = std::env::temp_dir().join(format!("mtide-install-hook-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"install-hook","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.on_install(function() ttt.log("installed") end)
ttt.on_uninstall(function() ttt.log("uninstalled") end)
ttt.log("loaded")
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime.logs().iter().any(|line| line == "[info] loaded"));
        assert!(!runtime.logs().iter().any(|line| line == "[info] installed"));
        assert!(runtime.run_install_hook().expect("install hook"));
        assert!(runtime.logs().iter().any(|line| line == "[info] installed"));
        assert!(!runtime.run_install_hook().expect("second install hook"));
        assert!(runtime.run_uninstall_hook().expect("uninstall hook"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] uninstalled"));
        assert!(!runtime.run_uninstall_hook().expect("second uninstall hook"));
        fs::remove_dir_all(root).expect("cleanup");
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
    fn markdown_widget_accepts_string_and_table_forms() {
        let lua = Lua::new();
        let string = lua.create_string("# Title\nSome **bold** and [link](https://example.com)");
        assert_eq!(
            markdown_widget_lines(&lua, LuaValue::String(string.expect("string")))
                .expect("string markdown"),
            vec!["Title", "Some bold and link"]
        );
        let table = lua.create_table().expect("table");
        table.set("text", "- item").expect("text");
        assert_eq!(
            markdown_widget_lines(&lua, LuaValue::Table(table)).expect("table markdown"),
            vec!["- item"]
        );
    }

    #[test]
    fn markdown_widget_preserves_fenced_code_content() {
        let lua = Lua::new();
        let markdown = lua
            .create_string("```rust\n# not a heading\nlet value = **literal**;\n```\n# Title")
            .expect("markdown");
        assert_eq!(
            markdown_widget_lines(&lua, LuaValue::String(markdown)).expect("fenced markdown"),
            vec![
                "```rust",
                "# not a heading",
                "let value = **literal**;",
                "```",
                "Title"
            ]
        );
    }

    #[test]
    fn markdown_widget_wraps_table_text_by_unicode_width() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        table.set("text", "alpha beta 中文").expect("text");
        table.set("wrap_width", 9_u64).expect("width");
        assert_eq!(
            markdown_widget_lines(&lua, LuaValue::Table(table)).expect("wrapped markdown"),
            vec!["alpha", "beta 中文"]
        );
    }

    #[test]
    fn label_widget_accepts_string_and_configuration_table_forms() {
        let lua = Lua::new();
        let string = lua.create_string("Status").expect("string");
        assert_eq!(
            label_widget_text(&lua, &LuaValue::String(string)).expect("string label"),
            "Status"
        );
        let table = lua.create_table().expect("table");
        table.set("text", "Containers").expect("text");
        table.set("badge", 3).expect("badge");
        table.set("padding_left", 1).expect("left padding");
        table.set("padding_right", 2).expect("right padding");
        table.set("margin_top", 1).expect("top margin");
        table.set("margin_bottom", 1).expect("bottom margin");
        assert_eq!(
            label_widget_text(&lua, &LuaValue::Table(table)).expect("table label"),
            "\n Containers  3  \n"
        );
        let title = lua.create_table().expect("title");
        title.set("text", "CONTAINERS").expect("title text");
        title.set("badge", "3").expect("title badge");
        assert_eq!(
            title_widget_text(&lua, &title).expect("table title"),
            "CONTAINERS  3"
        );
    }

    #[test]
    fn title_menu_renders_and_dispatches_command_callback() {
        let root = std::env::temp_dir().join(format!("mtide-title-menu-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"title-menu","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "menu", render = function(panel)
  panel:title({text = "Deploy", badge = "3", menu = {{label = "Refresh", command = "refresh"}}, on_menu = function(command) ttt.log("menu:" .. command) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("menu").expect("render"),
            Some("Deploy  3".into())
        );
        assert_eq!(runtime.panel_context_menu("menu"), vec!["Refresh"]);
        assert!(runtime
            .activate_panel_context_item("menu", 0, 1)
            .expect("activate menu"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] menu:refresh"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn box_widget_renders_child_panel_content() {
        let root = std::env::temp_dir().join(format!("mtide-box-widget-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"box-widget","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true,"editor.file_handlers":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "box", render = function(panel)
  panel:box({title = "Details", render = function(child) child:label("inside") end})
end}, file_handlers = {{id = "box.file", extensions = {"box"}, open = function(_path, tab)
  tab.render = function(panel)
    panel:box({title = "File", render = function(child) child:label("file-inside") end})
  end
end}}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("box").expect("render"),
            Some("inside".into())
        );
        let file = root.join("sample.box");
        fs::write(&file, b"content").expect("file");
        assert_eq!(
            runtime.open_file(&file).expect("file render"),
            Some("file-inside".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn multiline_panel_input_preserves_newlines_and_minimum_rows() {
        let root = std::env::temp_dir().join(format!(
            "mtide-panel-multiline-input-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-multiline-input","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "input", render = function(panel) panel:input({multiline = true, wrap = true, min_rows = 3, max_rows = 3, text = "one two three four", autofocus = true, on_change = function(text) ttt.log("changed") end, on_submit = function() ttt.log("submitted") end}) end}})"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.set_panel_size(10, 10);
        let rendered = runtime
            .render_panel("input")
            .expect("render")
            .expect("text");
        assert_eq!(rendered.split('\n').count(), 3);
        assert!(!rendered.contains("one two three four"));
        assert!(runtime.focus_autofocus_input("input"));
        assert!(runtime
            .panel_input_key("input", "Enter", None, "")
            .expect("newline"));
        assert!(runtime
            .panel_input_key("input", "Char", Some('t'), "")
            .expect("character"));
        assert!(runtime.logs().iter().any(|line| line == "[info] changed"));
        assert!(runtime.logs().iter().all(|line| line != "[info] submitted"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn drawer_compat_widgets_render_keyvalues_dividers_and_raw_cells() {
        let root = std::env::temp_dir().join(format!(
            "mtide-drawer-compat-widgets-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"drawer-compat-widgets","entry":"init.lua","api":2,"permissions":{"panel.drawer":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.open_drawer({render = function(panel)
  panel:title("Deploy")
  panel:keyvalue({{key = "Branch", value = "main"}})
  panel:divider()
  panel:button({label = "Go", on_click = function() ttt.log("drawer-go") end})
  panel:clear(0, 0, 10, 1)
  panel:input({text = "note", on_change = function(text) ttt.log("drawer-change:" .. text) end, on_submit = function(text) ttt.log("drawer-submit:" .. text) end})
  panel:checkbox({label = "Dry run", checked = false, on_change = function(value) ttt.log("drawer-check:" .. tostring(value)) end})
  panel:dropdown({options = {"one", "two"}, selected = 1, on_change = function(value) ttt.log("drawer-drop:" .. tostring(value)) end})
  panel:password({text = "secret", on_change = function(text) ttt.log("drawer-password:" .. text) end})
  panel:text(0, 0, "raw")
  panel:cell(0, 1, "*")
end})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("__drawer").expect("render"),
            Some(
                "raw       \n*ranch: main\n────────\n[ Go ]\n❯ note\n[ ] Dry run\n▼ one\n  two\n❯ ••••••"
                    .into()
            )
        );
        assert!(runtime.cycle_drawer_focus(false));
        assert!(runtime
            .drawer_input_key("Enter", None, "")
            .expect("focused drawer button"));
        assert!(runtime.logs().iter().any(|line| line == "[info] drawer-go"));
        assert!(runtime.activate_drawer_line(3).expect("button"));
        assert!(runtime.logs().iter().any(|line| line == "[info] drawer-go"));
        assert!(runtime.focus_drawer_line(4));
        assert!(runtime
            .drawer_input_key("Char", Some('!'), "")
            .expect("input"));
        assert!(runtime.drawer_input_key("Enter", None, "").expect("submit"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] drawer-change:note!"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] drawer-submit:note!"));
        assert!(runtime.focus_drawer_line(5));
        assert!(runtime
            .drawer_input_key("Enter", None, "")
            .expect("checkbox"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] drawer-check:true"));
        assert!(runtime.focus_drawer_line(6));
        assert!(runtime
            .drawer_input_key("Down", None, "")
            .expect("dropdown move"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] drawer-drop:2"));
        assert!(runtime.focus_drawer_line(7));
        assert!(runtime
            .drawer_password_key("Char", Some('!'), "")
            .expect("password input"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] drawer-password:secret!"));
        assert_eq!(
            runtime.render_panel("__drawer").expect("password render"),
            Some("raw       \n*ranch: main\n────────\n[ Go ]\n❯ note!\n[x] Dry run\n  one\n▼ two\n▶❯ •••••••".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_list_on_select_can_be_activated_after_render() {
        let root = std::env::temp_dir().join(format!("mtide-panel-select-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-select","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "items", render = function(panel)
  panel:list({items = {{label = "one"}, {label = "two"}}, on_select = function(item)
    ttt.log("selected:" .. item.label)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("items").expect("render"),
            Some("  one\n  two".into())
        );
        assert!(runtime
            .move_panel_selection("items", 1)
            .expect("move selection"));
        assert_eq!(
            runtime.render_panel("items").expect("render after move"),
            Some("  one\n▶ two".into())
        );
        assert!(runtime
            .activate_panel_selection("items", 1)
            .expect("activate"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] selected:two"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_scrollview_supports_selection_navigation() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-scrollview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-scrollview","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "scroll", render = function(panel)
  panel:scrollview({items = {"one", "two"}, selected = 1, on_select = function(item)
    ttt.log("scroll:" .. item)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("scroll").expect("render"),
            Some("▶ one\n  two".into())
        );
        assert!(runtime.focus_panel_scrollview("scroll", 0));
        assert!(runtime
            .panel_scrollview_key("scroll", "Down")
            .expect("move"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] scroll:two"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_radio_preserves_selection_and_calls_on_change() {
        let root = std::env::temp_dir().join(format!("mtide-panel-radio-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-radio","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "radio", render = function(panel)
  panel:radio({options = {"one", "two"}, selected = selected or 1,
    on_change = function(value) selected = value; ttt.log("radio:" .. value) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime
            .render_panel("radio")
            .expect("render")
            .unwrap()
            .contains("◉ one"));
        assert!(runtime.cycle_panel_focus("radio", false));
        assert!(runtime.panel_radio_key("radio", "Down").expect("move"));
        assert!(runtime.logs().iter().any(|line| line == "[info] radio:two"));
        let focused = runtime.render_panel("radio").expect("render").unwrap();
        assert!(focused.starts_with("▶"));
        assert!(focused.contains("◉ two"));
        assert!(runtime.panel_radio_key("radio", "Enter").expect("activate"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_image_has_text_fallback_without_graphics_protocol() {
        let root = std::env::temp_dir().join(format!("mtide-panel-image-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-image","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "image", render = function(panel)
  panel:image({src = "https://example.test/logo.png", alt = "Logo"})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("image").expect("render"),
            Some("🖼 Logo (https://example.test/logo.png)".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_image_accepts_embedded_binary_data_in_fallback() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        table.set("alt", "Screenshot").expect("alt");
        table
            .set("data", lua.create_string([0_u8, 1, 2, 255]).expect("data"))
            .expect("data field");
        assert_eq!(image_widget_text(&lua, &table), "🖼 Screenshot (4 bytes)");
    }

    #[test]
    fn panel_pager_preserves_offset_and_calls_on_change() {
        let root = std::env::temp_dir().join(format!("mtide-panel-pager-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-pager","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local offset = 0
ttt.register({sidebar = {id = "pager", render = function(panel)
  panel:pager({offset = offset, limit = 10, total = 25, on_change = function(new_offset)
    offset = new_offset
    ttt.log("offset:" .. new_offset)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("pager").expect("first render"),
            Some("« ‹ 1–10 of 25 › »".into())
        );
        assert!(runtime.move_panel_selection("pager", 1).expect("next page"));
        assert!(runtime.logs().iter().any(|line| line == "[info] offset:10"));
        assert_eq!(
            runtime.render_panel("pager").expect("second render"),
            Some("« ‹ 11–20 of 25 › »".into())
        );
        assert!(runtime
            .move_panel_horizontal_selection("pager", 1)
            .expect("last page"));
        assert!(runtime.logs().iter().any(|line| line == "[info] offset:20"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_tabs_support_active_id_and_preserve_it_after_selection() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-tabs-active-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-tabs-active","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local active = "details"
ttt.register({sidebar = {id = "tabs", render = function(panel)
  panel:tabs({items = {{id = "details", label = "Details"}, {id = "keys", label = "Keys"}}, active = active,
    on_change = function(id) active = id; ttt.log("active:" .. id) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("tabs").expect("first render"),
            Some("▶ Details\n  Keys".into())
        );
        assert!(runtime.cycle_panel_focus("tabs", false));
        assert!(runtime.panel_tabs_key("tabs", "Down").expect("select keys"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] active:keys"));
        let focused = runtime
            .render_panel("tabs")
            .expect("second render")
            .unwrap();
        assert!(focused.starts_with("▶"));
        assert!(focused.contains("▶ Keys"));
        assert!(runtime.panel_tabs_key("tabs", "Enter").expect("activate"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_widgets_keep_selection_state_isolated() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-widget-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-widget-state","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "mixed", render = function(panel)
  panel:list({items = {"one", "two"}})
  panel:tabs({items = {"A", "B"}, selected = 2, on_change = function(id)
    ttt.log("tab:" .. id)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("mixed").expect("render"),
            Some("  one\n  two\n  A\n▶ B".into())
        );
        assert!(runtime
            .move_panel_selection("mixed", -1)
            .expect("move tabs"));
        assert_eq!(
            runtime.render_panel("mixed").expect("render after move"),
            Some("  one\n  two\n▶ A\n  B".into())
        );
        assert!(runtime.logs().iter().any(|line| line == "[info] tab:A"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_dropdown_dispatches_on_change_from_selection_state() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-dropdown-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-dropdown","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "dropdown", render = function(panel)
  panel:dropdown({options = {"one", "two"}, selected = 1, on_change = function(value)
    ttt.log("changed:" .. value)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.render_panel("dropdown").expect("render");
        assert!(runtime.move_panel_selection("dropdown", 1).expect("move"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] changed:two"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_dropdown_participates_in_focus_cycle() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-dropdown-focus-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-dropdown-focus","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "dropdown-focus", render = function(panel)
  panel:dropdown({options = {"one", "two"}, selected = 1, on_change = function(value)
    ttt.log("dropdown:" .. tostring(value))
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("dropdown-focus").expect("render"),
            Some("▼ one\n  two".into())
        );
        assert!(runtime.cycle_panel_focus("dropdown-focus", false));
        assert!(runtime
            .panel_dropdown_key("dropdown-focus", "Down")
            .expect("move"));
        runtime.render_panel("dropdown-focus").expect("rerender");
        assert!(runtime
            .panel_dropdown_key("dropdown-focus", "Enter")
            .expect("activate"));
        let focused_render = runtime
            .render_panel("dropdown-focus")
            .expect("focused render")
            .expect("focused text");
        assert!(focused_render.starts_with("▶"), "{focused_render:?}");
        assert!(focused_render.contains("two"), "{focused_render:?}");
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] dropdown:two"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn list_key_commands_dispatch_selected_node_to_on_command() {
        let root =
            std::env::temp_dir().join(format!("mtide-list-key-command-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"list-key-command","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "list", render = function(panel)
  panel:list({items = {{id = "one", label = "One"}, {id = "two", label = "Two"}}, selected = 2,
    key_commands = {x = "open"}, on_command = function(command, node)
      ttt.log(command .. ":" .. node.id)
    end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.render_panel("list").expect("render");
        assert!(runtime
            .activate_panel_command("list", "x")
            .expect("command"));
        assert!(runtime.logs().iter().any(|line| line == "[info] open:two"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn table_key_commands_dispatch_selected_row_to_on_command() {
        let root =
            std::env::temp_dir().join(format!("mtide-table-key-command-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"table-key-command","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "table", render = function(panel)
  panel:table({rows = {{"one"}, {"two"}}, selected = 2, key_commands = {r = "restart"}, on_command = function(command, row)
    ttt.log(command .. ":" .. row)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime.render_panel("table").expect("render").is_some());
        assert!(runtime
            .activate_panel_command("table", "r")
            .expect("command"));
        assert!(runtime.logs().iter().any(|line| line == "[info] restart:2"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn table_columns_render_header_width_and_alignment() {
        let root = std::env::temp_dir().join(format!("mtide-table-columns-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"table-columns","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "table", render = function(panel)
  panel:table({columns = {{label = "Name"}, {label = "Status"}, {label = "Port", width = 6, align = "right"}, {label = "", actions = {{label = "Open", command = "open"}}}}, rows = {{"web-app", "running", "8080"}, {"database", "stopped", "5432"}}, on_command = function(command, row) ttt.log(command .. ":" .. row) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let rendered = runtime
            .render_panel("table")
            .expect("render")
            .expect("table output");
        assert!(rendered
            .lines()
            .next()
            .is_some_and(|line| line.contains("Name")
                && line.contains("Status")
                && line.contains("Port")));
        assert!(rendered.contains("web-app"));
        assert!(rendered.contains("running"));
        assert!(rendered.contains("Open"), "{rendered:?}");
        assert!(rendered.lines().any(|line| line.contains("  8080  Open")));
        let action_x = rendered
            .lines()
            .nth(1)
            .and_then(|line| line.find("Open"))
            .expect("action position")
            + 1;
        assert!(runtime
            .activate_panel_action("table", action_x, 1)
            .expect("action"));
        assert!(runtime.logs().iter().any(|line| line == "[info] open:1"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn table_loading_keeps_existing_rows_and_marks_the_header() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        let columns = lua.create_table().expect("columns");
        let column = lua.create_table().expect("column");
        column.set("label", "Name").expect("label");
        columns.set(1, column).expect("column");
        let rows = lua.create_table().expect("rows");
        let row = lua.create_table().expect("row");
        row.set(1, "still visible").expect("cell");
        rows.set(1, row).expect("row");
        table.set("columns", columns).expect("columns");
        table.set("rows", rows).expect("rows");
        table.set("loading", true).expect("loading");
        let rendered =
            collection_widget_text_with_width(&lua, LuaValue::Table(table), true, Some(80));
        assert!(rendered
            .lines()
            .next()
            .is_some_and(|line| line.contains("loading")));
        assert!(rendered.contains("still visible"));
    }

    #[test]
    fn table_horizontal_scroll_pins_action_column() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        table.set("scroll_x", 4_u64).expect("scroll");
        let rendered =
            horizontal_table_slice_with_pinned_action(&table, "  Name      Status     Open", 16, 4);
        assert!(rendered.ends_with("  Open"), "{rendered:?}");
        assert_eq!(UnicodeWidthStr::width(rendered.as_str()), 16);
    }

    #[test]
    fn table_fixed_column_width_is_not_expanded_by_long_cells() {
        let lua = Lua::new();
        let table = lua.create_table().expect("table");
        let columns = lua.create_table().expect("columns");
        let column = lua.create_table().expect("column");
        column.set("label", "Name").expect("label");
        column.set("width", 4_u64).expect("width");
        columns.set(1, column).expect("column");
        let rows = lua.create_table().expect("rows");
        let row = lua.create_table().expect("row");
        row.set(1, "long value").expect("cell");
        rows.set(1, row).expect("row");
        table.set("columns", columns).expect("columns");
        table.set("rows", rows).expect("rows");
        let rendered =
            collection_widget_text_with_width(&lua, LuaValue::Table(table), true, Some(80));
        assert!(rendered.contains("lon…"), "{rendered:?}");
        assert!(!rendered.contains("long value"), "{rendered:?}");
    }

    #[test]
    fn regular_panel_button_click_invokes_on_click_repeatedly() {
        let root = std::env::temp_dir().join(format!("mtide-panel-button-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-button","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "buttons", render = function(panel)
  panel:button({label = "Run", on_click = function() ttt.log("clicked") end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("buttons").expect("render"),
            Some("[ Run ]".into())
        );
        assert!(runtime.activate_panel_button("buttons", 0).expect("click"));
        assert!(runtime
            .activate_panel_button("buttons", 0)
            .expect("second click"));
        assert!(runtime
            .panel_button_key("buttons", "Enter", None)
            .expect("keyboard click"));
        assert_eq!(
            runtime
                .logs()
                .iter()
                .filter(|line| *line == "[info] clicked")
                .count(),
            3
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn disabled_panel_button_is_rendered_but_not_focusable_or_activatable() {
        let root = std::env::temp_dir().join(format!(
            "mtide-panel-disabled-button-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-disabled-button","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "buttons", render = function(panel)
  panel:button({label = "Busy", disabled = true, on_click = function() ttt.log("clicked") end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("buttons").expect("render"),
            Some("[ Busy ]".into())
        );
        assert!(!runtime
            .activate_panel_button("buttons", 0)
            .expect("disabled click"));
        assert!(!runtime.cycle_panel_focus("buttons", false));
        assert!(runtime.logs().iter().all(|line| !line.contains("clicked")));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_input_preserves_text_and_dispatches_callbacks() {
        let root = std::env::temp_dir().join(format!("mtide-panel-input-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-input","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "input", render = function(panel)
  panel:input({text = "old", prefix = "$ ", autofocus = true, on_change = function(text) ttt.log("change:" .. text) end, on_submit = function(text) ttt.log("submit:" .. text) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("input").expect("render"),
            Some("$ ❯ old".into())
        );
        assert!(runtime.focus_autofocus_input("input"));
        assert!(runtime
            .panel_input_key("input", "Char", Some('!'), "")
            .expect("change"));
        assert!(runtime
            .panel_input_key("input", "Enter", None, "")
            .expect("submit"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] change:old!"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] submit:old!"));
        assert_eq!(
            runtime.render_panel("input").expect("rerender"),
            Some("$ ❯ old!".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn plugin_input_editing_supports_unicode_cursor_navigation_and_delete() {
        let mut text = "猫🙂尾".to_owned();
        let mut cursor = text.chars().count();
        assert_eq!(
            edit_plugin_input(&mut text, &mut cursor, "Left", None, false),
            Some(false)
        );
        assert_eq!(
            edit_plugin_input(&mut text, &mut cursor, "Char", Some('中'), false),
            Some(true)
        );
        assert_eq!(text, "猫🙂中尾");
        assert_eq!(cursor, 3);
        assert_eq!(
            edit_plugin_input(&mut text, &mut cursor, "Delete", None, false),
            Some(true)
        );
        assert_eq!(text, "猫🙂中");
        assert_eq!(
            edit_plugin_input(&mut text, &mut cursor, "Home", None, false),
            Some(false)
        );
        assert_eq!(cursor, 0);
        assert_eq!(
            edit_plugin_input(&mut text, &mut cursor, "End", None, false),
            Some(false)
        );
        assert_eq!(cursor, text.chars().count());
    }

    #[test]
    fn controlled_panel_input_uses_value_from_each_render() {
        let root = std::env::temp_dir().join(format!(
            "mtide-panel-controlled-input-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-controlled-input","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local value = "one"
ttt.register({sidebar = {id = "input", render = function(panel)
  panel:input({text = "stale", value = value, on_change = function(text) value = text end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("input").expect("initial render"),
            Some("❯ one".into())
        );
        assert!(runtime.focus_panel_input("input", 0));
        assert!(runtime
            .panel_input_key("input", "Char", Some('!'), "")
            .expect("controlled change"));
        assert_eq!(
            runtime.render_panel("input").expect("controlled rerender"),
            Some("❯ one!".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_password_masks_text_and_dispatches_changes() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-password-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-password","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "password", render = function(panel)
  panel:password({text = "old", on_change = function(text) ttt.log("change:" .. text) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("password").expect("render"),
            Some("❯ •••".into())
        );
        assert!(runtime.focus_panel_password("password", 0));
        assert!(runtime
            .panel_password_key("password", "Char", Some('!'), "")
            .expect("change"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] change:old!"));
        assert_eq!(
            runtime.render_panel("password").expect("rerender"),
            Some("▶❯ ••••".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_controls_cycle_focus_with_tab() {
        let root = std::env::temp_dir().join(format!("mtide-panel-focus-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-focus","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "focus", render = function(panel)
  panel:input({text = "input"})
  panel:checkbox({label = "check"})
  panel:button({label = "run", on_click = function() ttt.log("clicked") end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.render_panel("focus").expect("render");
        assert!(runtime.cycle_panel_focus("focus", false));
        assert!(runtime
            .panel_input_key("focus", "Char", Some('!'), "")
            .is_ok());
        assert!(runtime.cycle_panel_focus("focus", false));
        assert!(runtime
            .panel_checkbox_key("focus", "Enter", None)
            .expect("checkbox"));
        assert!(runtime.cycle_panel_focus("focus", false));
        assert!(runtime
            .panel_button_key("focus", "Enter", None)
            .expect("button"));
        assert!(runtime.cycle_panel_focus("focus", true));
        assert!(runtime
            .panel_checkbox_key("focus", "Enter", None)
            .expect("reverse checkbox"));
        assert!(runtime.logs().iter().any(|line| line == "[info] clicked"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_mouse_focus_is_mutually_exclusive() {
        let root = std::env::temp_dir().join(format!(
            "mtide-panel-focus-exclusive-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-focus-exclusive","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "exclusive", render = function(panel)
  panel:input({text = "input"})
  panel:checkbox({label = "check"})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.render_panel("exclusive").expect("render");
        assert!(runtime.focus_panel_input("exclusive", 0));
        assert!(runtime.focus_panel_checkbox("exclusive", 1));
        assert!(!runtime
            .panel_input_key("exclusive", "Char", Some('!'), "")
            .expect("input should not consume after losing focus"));
        assert!(runtime
            .panel_checkbox_key("exclusive", "Enter", None)
            .expect("checkbox should retain focus"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_mouse_click_focuses_and_activates_controls() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-mouse-controls-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-mouse-controls","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "mouse", render = function(panel)
  panel:input({value = "value"})
  panel:checkbox({label = "Ready", checked = false, on_change = function(value) ttt.log(tostring(value)) end})
  panel:button({label = "Run", on_click = function() ttt.log("clicked") end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.render_panel("mouse").expect("render");

        assert!(runtime
            .activate_panel_control_at("mouse", 0)
            .expect("input click"));
        assert!(runtime
            .panel_input_key("mouse", "Char", Some('!'), "")
            .expect("input key"));
        assert!(runtime
            .activate_panel_control_at("mouse", 1)
            .expect("checkbox click"));
        assert!(runtime
            .activate_panel_control_at("mouse", 2)
            .expect("button click"));
        assert!(runtime.logs().iter().any(|line| line == "[info] true"));
        assert!(runtime.logs().iter().any(|line| line == "[info] clicked"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn list_row_action_click_dispatches_command() {
        let root = std::env::temp_dir().join(format!("mtide-row-action-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"row-action","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "actions", render = function(panel)
  panel:list({items = {{label = "one", actions = {{label = "Open", command = "open"}}}}, on_command = function(command, node)
    ttt.log(command .. ":" .. node.label)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("actions").expect("render"),
            Some("  one  [Open]".into())
        );
        assert!(runtime
            .activate_panel_action("actions", 10, 0)
            .expect("action"));
        assert!(runtime.logs().iter().any(|line| line == "[info] open:one"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_checkbox_toggles_and_calls_on_change() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-checkbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-checkbox","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "checkbox", render = function(panel)
  panel:checkbox({label = "Enabled", checked = false, on_change = function(value) ttt.log(tostring(value)) end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("checkbox").expect("render"),
            Some("[ ] Enabled".into())
        );
        assert!(runtime
            .activate_panel_checkbox("checkbox", 0)
            .expect("click"));
        assert!(runtime
            .panel_checkbox_key("checkbox", "Enter", None)
            .expect("keyboard"));
        assert!(runtime.logs().iter().any(|line| line == "[info] true"));
        assert!(runtime.logs().iter().any(|line| line == "[info] false"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regular_panel_controls_do_not_register_drawer_state() {
        let root = std::env::temp_dir().join(format!(
            "mtide-panel-control-isolation-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-control-isolation","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "regular", render = function(panel)
  panel:button({label = "Run", on_click = function() end})
  panel:input({value = "value", on_change = function() end})
  panel:checkbox({label = "Ready", checked = true, on_change = function() end})
  panel:dropdown({options = {"one", "two"}, selected = 1, on_change = function() end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime.render_panel("regular").expect("render").is_some());
        assert!(!runtime.focus_drawer_line(0));
        assert!(!runtime
            .drawer_input_key("Char", Some('x'), "")
            .expect("input key"));
        assert!(!runtime.activate_drawer_line(0).expect("activate"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_tabs_on_select_can_be_activated_after_render() {
        let root = std::env::temp_dir().join(format!("mtide-panel-tabs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-tabs","entry":"init.lua","api":2,"permissions":{"panel.bottom":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({bottom = {id = "tabs", render = function(panel)
  panel:tabs({items = {"One", "Two"}, selected = 1, on_change = function(id)
    ttt.log("tab:" .. id)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("tabs").expect("render"),
            Some("▶ One\n  Two".into())
        );
        assert!(runtime
            .activate_panel_selection("tabs", 1)
            .expect("activate"));
        assert!(runtime.logs().iter().any(|line| line == "[info] tab:Two"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn registered_panel_on_event_receives_key() {
        let root = std::env::temp_dir().join(format!("mtide-panel-event-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-event","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "events", render = function(panel)
  panel:label("panel")
end, on_event = function(event)
  if event.type == "key" and event.key == "x" then
    ttt.log("panel-event:" .. event.rune)
    return true
  end
  if event.key == "y" then error("broken panel handler") end
  if event.type == "mouse" and event.x == 3 and event.y == 4 and event.button == "left" then
    ttt.log("panel-mouse")
    return true
  end
  return false
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("events").expect("render"),
            Some("panel".into())
        );
        assert!(runtime
            .emit_panel_key_event("events", "x", Some('x'), "")
            .expect("event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] panel-event:x"));
        assert!(!runtime
            .emit_panel_key_event("events", "y", Some('y'), "")
            .expect("event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line.contains("panel on_event failed")));
        assert!(runtime
            .emit_panel_mouse_event("events", 3, 4, "left")
            .expect("mouse event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] panel-mouse"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn gutter_click_event_requires_bookmark_permission_and_receives_location() {
        let root = std::env::temp_dir().join(format!("mtide-gutter-event-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"gutter-event","entry":"init.lua","api":2,"permissions":{"editor.bookmarks":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local events = require("ttt.events")
events.on("gutter.click", function(path, line)
  ttt.log(path .. ":" .. line)
end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime
            .emit_with_payload(
                "gutter.click",
                &[
                    PluginEventArg::Text("src/main.rs".to_owned()),
                    PluginEventArg::Number(7),
                ],
            )
            .expect("event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] src/main.rs:7"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn open_tab_on_event_receives_key_and_close_without_on_close_succeeds() {
        let root = std::env::temp_dir().join(format!("mtide-tab-event-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"tab-event","entry":"init.lua","api":2,"permissions":{"panel.editor":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.open_tab({id = "events", title = "Events", render = function(panel)
  panel:label("event tab")
end, on_event = function(event)
  if event.type == "key" and event.key == "x" then
    ttt.log("event-key:" .. event.rune)
    return true
  end
  if event.type == "mouse" and event.x == 2 and event.y == 3 and event.button == "left" then
    ttt.log("event-mouse")
    return true
  end
  if event.key == "y" then error("broken tab handler") end
  return false
end})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("__tab:events").expect("render"),
            Some("event tab".into())
        );
        assert!(runtime
            .emit_tab_key_event("events", "x", Some('x'), "")
            .expect("event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] event-key:x"));
        assert!(runtime
            .emit_tab_mouse_event("events", 2, 3, "left")
            .expect("mouse event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] event-mouse"));
        assert!(!runtime
            .emit_tab_key_event("events", "y", Some('y'), "")
            .expect("event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line.contains("tab on_event failed")));
        assert!(runtime.close_tab("events").expect("close"));
        assert_eq!(
            runtime.render_panel("__tab:events").expect("closed render"),
            None
        );
        assert!(!runtime
            .emit_tab_key_event("events", "x", Some('x'), "")
            .expect("closed event"));
        assert!(!runtime.close_tab("events").expect("close missing"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn custom_tab_and_file_view_mouse_controls_use_panel_widget_dispatch() {
        let root =
            std::env::temp_dir().join(format!("mtide-view-mouse-controls-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"view-mouse-controls","entry":"init.lua","api":2,"permissions":{"panel.editor":true,"editor.file_handlers":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.open_tab({id = "custom", title = "Custom", render = function(panel)
  panel:button({label = "Tab", on_click = function() ttt.log("tab-click") end})
end})
ttt.register({file_handlers = {{id = "view.file", extensions = {"vwm"}, open = function(_path, tab)
  tab.render = function(panel)
    panel:button({label = "File", on_click = function() ttt.log("file-click") end})
  end
end}}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("__tab:custom").expect("tab render"),
            Some("[ Tab ]".into())
        );
        assert!(runtime
            .activate_panel_control_at("__tab:custom", 0)
            .expect("tab click"));

        let file = root.join("sample.vwm");
        fs::write(&file, "content").expect("file");
        assert_eq!(
            runtime.open_file(&file).expect("file open"),
            Some("[ File ]".into())
        );
        let file_id = runtime.take_file_close_id().expect("file id");
        assert!(runtime
            .activate_panel_control_at(&format!("__file:{file_id}"), 0)
            .expect("file click"));
        assert!(runtime.logs().iter().any(|line| line == "[info] tab-click"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] file-click"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn file_handler_tab_on_event_receives_key_and_is_cleaned_on_close() {
        let root =
            std::env::temp_dir().join(format!("mtide-file-tab-event-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"file-tab-event","entry":"init.lua","api":2,"permissions":{"editor.file_handlers":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({file_handlers = {{id = "event.file", extensions = {"evt"}, open = function(_path, tab)
  tab.on_event = function(event)
    if event.type == "key" and event.key == "x" then
      ttt.log("file-event:" .. event.rune)
      return true
    end
    if event.type == "mouse" and event.x == 4 and event.y == 5 and event.button == "wheel_down" then
      ttt.log("file-mouse")
      return true
    end
    if event.key == "y" then error("broken file handler") end
    return false
  end
  return "file view"
end}}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let path = root.join("sample.evt");
        fs::write(&path, "payload").expect("file");
        assert_eq!(
            runtime.open_file(&path).expect("open"),
            Some("file view".into())
        );
        let close_id = runtime.take_file_close_id().expect("file tab id");
        assert!(runtime
            .emit_file_key_event(close_id, "x", Some('x'), "")
            .expect("event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] file-event:x"));
        assert!(runtime
            .emit_file_mouse_event(close_id, 4, 5, "wheel_down")
            .expect("mouse event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] file-mouse"));
        assert!(!runtime
            .emit_file_key_event(close_id, "y", Some('y'), "")
            .expect("event error"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line.contains("file tab on_event failed")));
        runtime.close_file(close_id).expect("close");
        assert!(!runtime
            .emit_file_key_event(close_id, "x", Some('x'), "")
            .expect("event after close"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn file_handler_tab_focus_and_close_enqueue_host_requests() {
        let root =
            std::env::temp_dir().join(format!("mtide-file-tab-lifecycle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"file-tab-lifecycle","entry":"init.lua","api":2,"permissions":{"editor.file_handlers":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({file_handlers = {{id = "lifecycle.file", extensions = {"life"}, open = function(_path, tab)
  tab:focus()
  tab:close()
  return "file view"
end}}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let path = root.join("sample.life");
        fs::write(&path, "payload").expect("file");
        assert_eq!(
            runtime.open_file(&path).expect("open"),
            Some("file view".into())
        );
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![
                PluginPanelRequest::FocusFileView,
                PluginPanelRequest::CloseFileView,
            ]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn file_handler_magic_match_wins_over_extension_match() {
        let root =
            std::env::temp_dir().join(format!("mtide-file-handler-magic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"file-handler-magic","entry":"init.lua","api":2,"permissions":{"editor.file_handlers":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({file_handlers = {
  {id = "extension", extensions = {"bin"}, open = function() return "extension" end},
  {id = "magic", magic = "MAGIC-", open = function() return "magic" end}
}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let path = root.join("sample.bin");
        fs::write(&path, b"MAGIC-payload").expect("file");
        assert_eq!(
            runtime.open_file(&path).expect("open"),
            Some("magic".into())
        );
        assert_eq!(runtime.take_file_title().as_deref(), Some("magic"));

        let no_extension = root.join("sample");
        fs::write(&no_extension, b"MAGIC-payload").expect("file");
        assert_eq!(
            runtime
                .open_file(&no_extension)
                .expect("open without extension"),
            Some("magic".into())
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn lifecycle_event_permissions_are_enforced_for_events_module() {
        let root =
            std::env::temp_dir().join(format!("mtide-event-permission-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"event-permission","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local events = require("ttt.events")
local ok, err = pcall(function()
  events.on("file.open", function() end)
end)
assert(not ok and tostring(err):find("permission denied for event", 1, true) ~= nil)
events.on("plugin.custom", function() end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        PluginRuntime::load(&package).expect("runtime");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn event_dispatch_snapshots_callbacks_before_invoking_lua() {
        let root = std::env::temp_dir().join(format!(
            "mtide-event-dispatch-snapshot-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"event-snapshot","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local events = require("ttt.events")
local registered = false
events.on("probe", function()
  if not registered then
    registered = true
    events.on("probe", function() end)
  end
end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime.emit("probe").expect("first dispatch"));
        assert!(runtime.emit("probe").expect("second dispatch"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn event_dispatch_continues_after_callback_error() {
        let root =
            std::env::temp_dir().join(format!("mtide-event-dispatch-error-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"event-error","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local events = require("ttt.events")
events.on("probe", function() ttt.log("first callback entered"); error("first callback failed") end)
events.on("probe", function() ttt.log("second callback ran") end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let result = runtime.emit("probe");
        assert!(result.is_err());
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] second callback ran"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn key_press_event_can_consume_editor_input() {
        let root =
            std::env::temp_dir().join(format!("mtide-key-press-event-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"key-press-event","entry":"init.lua","api":2,"permissions":{"keybindings":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local events = require("ttt.events")
events.on("key.press", function(event)
  if event.key == "j" then
    assert(event.type == "key" and event.rune == "j")
    assert(event.mod == nil)
    return true
  end
  return false
end)
events.on("key.press", function()
  error("broken key handler")
end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime
            .emit_key_event("j", Some('j'), "")
            .expect("key event"));
        assert!(!runtime
            .emit_key_event("k", Some('k'), "")
            .expect("unconsumed key event"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line.contains("key event handler failed")));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn file_handler_panel_redraw_refreshes_rendered_view() {
        let root =
            std::env::temp_dir().join(format!("mtide-file-panel-redraw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"file-panel-redraw","entry":"init.lua","api":2,"permissions":{"editor.file_handlers":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local text = "first"
ttt.register({file_handlers = {{id = "redraw.file", extensions = {"rdr"}, open = function(_path, tab)
  tab.on_event = function(event)
    if event.key == "x" then
      text = "second"
      tab.panel:redraw()
      return true
    end
    return false
  end
  tab.render = function(panel)
    tab.panel = panel
    panel:label(text)
  end
end}}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let path = root.join("sample.rdr");
        fs::write(&path, "payload").expect("file");
        assert_eq!(
            runtime.open_file(&path).expect("open"),
            Some("first".into())
        );
        let close_id = runtime.take_file_close_id().expect("file id");
        assert!(runtime
            .emit_file_key_event(close_id, "x", Some('x'), "")
            .expect("event"));
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::RefreshFileView { id: close_id }]
        );
        assert_eq!(
            runtime.render_file_view(close_id).expect("redraw"),
            Some("second".into())
        );
        runtime.close_file(close_id).expect("close");
        assert_eq!(
            runtime.render_file_view(close_id).expect("closed redraw"),
            None
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_redraw_enqueues_host_refresh_request() {
        let root = std::env::temp_dir().join(format!("mtide-panel-redraw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-redraw","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "redraw", title = "Redraw", render = function(panel)
  assert(type(panel.redraw) == "function")
  panel:redraw()
  panel:label("panel")
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("redraw").expect("render"),
            Some("panel".into())
        );
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::RefreshPanel {
                id: "redraw".into()
            }]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_tree_on_select_receives_node_after_render() {
        let root = std::env::temp_dir().join(format!("mtide-panel-tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-tree","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "tree", render = function(panel)
  panel:tree({items = {{id = "src", label = "src"}, {id = "main", label = "main.rs"}}, node_menu = {{label = "Open", command = "open", checked = true}, {separator = true}}, key_commands = {x = "open"}, on_command = function(command, node)
    ttt.log(command .. ":" .. node.id)
  end, on_select = function(node)
    ttt.log("node:" .. node.id)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.render_panel("tree").expect("render"),
            Some("  src\n  main.rs".into())
        );
        assert_eq!(
            runtime.panel_context_menu("tree"),
            vec!["✓ Open", "────────"]
        );
        assert!(runtime
            .activate_panel_context_item("tree", 0, 1)
            .expect("context menu"));
        assert!(runtime.logs().iter().any(|line| line == "[info] open:src"));
        assert!(runtime
            .activate_panel_selection("tree", 1)
            .expect("activate"));
        assert!(runtime.logs().iter().any(|line| line == "[info] node:main"));
        runtime
            .render_panel("tree")
            .expect("rerender for leaf activation");
        assert!(runtime.set_panel_selected_row("tree", 1));
        runtime
            .render_panel("tree")
            .expect("rerender selected leaf");
        assert!(runtime
            .activate_panel_current_selection("tree")
            .expect("leaf activate"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] activate:main"));
        assert!(runtime
            .activate_panel_command("tree", "x")
            .expect("command"));
        assert!(runtime.logs().iter().any(|line| line == "[info] open:src"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_tree_enter_selects_and_expands_and_left_right_navigates() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-tree-expand-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-tree-expand","entry":"init.lua","api":2,"permissions":{"panel.sidebar":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.register({sidebar = {id = "tree", render = function(panel)
  panel:tree({items = {{id = "src", label = "src", children = {{id = "main", label = "main.rs"}}}}, on_expand = function(node)
    ttt.log("expanded:" .. node.id .. ":" .. tostring(node.expanded))
  end, on_select = function(node)
    ttt.log("selected:" .. node.id)
  end})
end}})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        runtime.render_panel("tree").expect("render");
        assert!(runtime
            .activate_panel_selection("tree", 0)
            .expect("activate"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] expanded:src:true"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] selected:src"));
        assert!(runtime
            .move_tree_horizontal_selection("tree", false)
            .expect("collapse"));
        assert!(runtime
            .move_tree_horizontal_selection("tree", true)
            .expect("expand"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn panel_table_on_activate_is_distinct_from_selection() {
        let root =
            std::env::temp_dir().join(format!("mtide-panel-activate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"panel-activate","entry":"init.lua","api":2,"permissions":{"panel.editor":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.open_tab({id = "rows", title = "Rows", render = function(panel)
  panel:table({rows = {{cells = {"one"},}, {cells = {"two"},}}, on_select = function(index)
    ttt.log("select:" .. index)
  end, on_activate = function(index)
    ttt.log("activate:" .. index)
  end})
end})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime
            .render_panel("__tab:rows")
            .expect("render")
            .is_some());
        assert!(runtime
            .activate_panel_row("__tab:rows", 1)
            .expect("activate"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] activate:2"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn status_item_on_click_can_be_activated() {
        let root = std::env::temp_dir().join(format!("mtide-status-click-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"status-click","entry":"init.lua","api":2,"permissions":{"commands":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
ttt.set_status_item("right", "build", "Build", {on_click = function()
  ttt.log("status-clicked")
end})
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime.activate_status_item("build").expect("activate"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] status-clicked"));
        fs::remove_dir_all(root).expect("cleanup");
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
assert(ttt.open_changes({id = "review", title = "Review", description = "summary", file_path = "a.txt", diff = "diff --git a/a.txt b/a.txt\n@@ -1 +1 @@\n-old\n+new"}) == "review")
"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.drain_panel_requests(),
            vec![PluginPanelRequest::OpenChangesTab {
                id: "review".into(),
                title: "Review".into(),
                file_path: Some("a.txt".into()),
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
local official = sys.spawn("/bin/sh", {args = {"-c", "printf official"}, on_stdout = function(chunk) ttt.log("official:" .. chunk) end})
assert(official:pid() > 0)
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
                && logs.iter().any(|line| line == "[info] official:official")
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
        let logs = runtime.logs();
        let stdout_index = logs
            .iter()
            .position(|line| line == "[info] out:spawned")
            .expect("stdout log position");
        let stderr_index = logs
            .iter()
            .position(|line| line == "[info] err:warning")
            .expect("stderr log position");
        let exit_index = logs
            .iter()
            .position(|line| line == "[info] exit:0")
            .expect("exit log position");
        assert!(exit_index > stdout_index);
        assert!(exit_index > stderr_index);
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] official:official"));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_runtime_terminates_registered_spawn_processes() {
        let root = std::env::temp_dir().join(format!("mtide-spawn-drop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"spawn-drop","entry":"init.lua","api":2,"permissions":{"system.exec":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local ttt = require("ttt")
local sys = require("ttt.system")
local proc = sys.spawn({shell = "/bin/sh", args = {"-c", "exec sleep 30"}})
ttt.log(tostring(proc:pid()))"#,
        )
        .expect("script");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        let pid = runtime
            .logs()
            .iter()
            .find_map(|line| line.strip_prefix("[info] ")?.parse::<u32>().ok())
            .expect("spawn pid log");
        drop(runtime);
        let still_running = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("kill probe")
            .success();
        assert!(!still_running, "spawn process {pid} survived runtime drop");
        fs::remove_dir_all(root).expect("cleanup");
    }

    fn write_rpc_message_for_test(stream: &mut impl Write, message: &JsonValue) {
        let encoded = serde_json::to_vec(message).expect("encode message");
        write!(stream, "Content-Length: {}\r\n\r\n", encoded.len()).expect("write header");
        stream.write_all(&encoded).expect("write message");
        stream.flush().expect("flush message");
    }

    #[test]
    fn rpc_endpoint_scope_accepts_only_declared_targets() {
        let allowed = vec!["127.0.0.1:4250".to_owned(), "unix:/tmp/ttt.sock".to_owned()];
        assert!(rpc_endpoint_allowed(
            "tcp://127.0.0.1:4250",
            &allowed,
            false
        ));
        assert!(rpc_endpoint_allowed("127.0.0.1:4250", &allowed, false));
        assert!(rpc_endpoint_allowed("unix:/tmp/ttt.sock", &allowed, false));
        assert!(!rpc_endpoint_allowed(
            "tcp://127.0.0.1:4251",
            &allowed,
            false
        ));
        assert!(!rpc_endpoint_allowed(
            "unix:/tmp/other.sock",
            &allowed,
            false
        ));
        assert!(!rpc_endpoint_allowed("tcp://127.0.0.1:4251", &[], false));
        assert!(rpc_endpoint_allowed("tcp://127.0.0.1:4251", &[], true));
    }

    #[test]
    fn system_exec_scope_accepts_only_declared_commands() {
        let allowed = vec!["git".to_owned(), "/bin/echo".to_owned()];
        assert!(command_allowed(&["git".to_owned()], &allowed, false));
        assert!(command_allowed(&["/bin/echo".to_owned()], &allowed, false));
        assert!(!command_allowed(&["/bin/sh".to_owned()], &allowed, false));
        assert!(command_allowed(&["/bin/sh".to_owned()], &[], true));
        assert!(!command_allowed(&["/bin/sh".to_owned()], &[], false));
    }

    #[test]
    fn fs_scope_allows_declared_workspace_and_plugin_roots_only() {
        let root = std::env::temp_dir().join(format!("mtide-fs-scope-{}", std::process::id()));
        let workspace = root.join("workspace");
        let package = root.join("plugin");
        let outside = root.join("outside");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(workspace.join("src")).expect("workspace");
        fs::create_dir_all(&package).expect("package");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(workspace.join("src/main.rs"), "fn main() {}").expect("file");
        fs::write(package.join("state.json"), "{}").expect("plugin file");
        let roots = fs_scope_roots(
            &package,
            &["{workspace}".to_owned()],
            false,
            Some(&workspace),
            None,
        );
        assert!(fs_path_allowed(&workspace.join("src/main.rs"), &roots));
        assert!(fs_path_allowed(&package.join("state.json"), &roots));
        assert!(!fs_path_allowed(&outside.join("secret"), &roots));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn plugin_workspace_scope_expands_for_runtime_workspace_root() {
        let root = std::env::temp_dir().join(format!(
            "mtide-plugin-dynamic-workspace-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let added_workspace = root.join("added-workspace");
        let package_root = root.join("plugin");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&added_workspace).expect("added workspace");
        fs::create_dir_all(&package_root).expect("package");
        fs::write(
            package_root.join("plugin.ttt.json"),
            r#"{"name":"dynamic-workspace","entry":"init.lua","api":2,"permissions":{"commands":true,"fs.write":["{workspace}"]}}"#,
        )
        .expect("manifest");
        fs::write(
            package_root.join("init.lua"),
            r#"local ttt = require("ttt")
local fs = require("ttt.fs")
ttt.register({commands = {{id = "write-added", title = "Write", handler = function()
  assert(fs.write("../added-workspace/result.txt", "ok") == true)
end}}})"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&package_root).expect("package");
        let runtime =
            PluginRuntime::load_with_settings_path_and_workspace(&package, None, Some(&workspace))
                .expect("runtime");
        runtime.add_workspace_root(&added_workspace);
        assert!(runtime.execute_command("write-added").expect("command"));
        assert_eq!(
            fs::read_to_string(added_workspace.join("result.txt")).unwrap(),
            "ok"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn plugin_config_scope_writes_to_config_directory_not_package_directory() {
        let root =
            std::env::temp_dir().join(format!("mtide-plugin-config-scope-{}", std::process::id()));
        let config = root.join("config");
        let package_root = root.join("plugin");
        fs::create_dir_all(config.join("themes")).expect("config themes");
        fs::create_dir_all(&package_root).expect("package");
        let settings_file = config.join("settings.json");
        fs::write(&settings_file, "{}").expect("settings");
        fs::write(
            package_root.join("plugin.ttt.json"),
            r#"{"name":"config-scope","entry":"init.lua","api":2,"permissions":{"commands":true,"fs.write":["{config}/themes"]}}"#,
        )
        .expect("manifest");
        fs::write(
            package_root.join("init.lua"),
            r#"local ttt=require("ttt"); local fs=require("ttt.fs"); ttt.register({commands={{id="write-theme",title="Write",handler=function() assert(fs.write("{config}/themes/omarchy.json", "{}")) end}}})"#,
        )
        .expect("entry");

        let package = PluginPackage::load(&package_root).expect("package");
        let runtime = PluginRuntime::load_with_settings_path_and_workspace(
            &package,
            Some(&settings_file),
            None,
        )
        .expect("runtime");
        assert!(runtime.execute_command("write-theme").expect("command"));
        assert_eq!(
            fs::read_to_string(config.join("themes/omarchy.json")).unwrap(),
            "{}"
        );
        assert!(!package_root.join("themes/omarchy.json").exists());
        fs::remove_dir_all(root).expect("cleanup");
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
            let mut reader = BufReader::new(RpcTransport::Unix(stream.try_clone().expect("clone")));
            let request = read_rpc_message(&mut reader)
                .expect("request frame")
                .expect("request");
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
            settings_keys: Vec::new(),
            network_http_hosts: Vec::new(),
            network_http_allow_all: false,
            grammars: Vec::new(),
            themes: Vec::new(),
        };
        assert!(m.validate(Path::new("/tmp/plugin")).is_err());
    }

    #[test]
    fn settings_helpers_support_scoped_dot_keys() {
        let mut settings = json!({"formatters": {"go": "gofmt"}});
        let allowed = vec!["formatters.*".to_owned()];
        assert!(setting_allowed("formatters.go", &allowed));
        assert!(!setting_allowed("lsp.server", &allowed));
        assert_eq!(
            setting_value(&settings, "formatters.go"),
            Some(&json!("gofmt"))
        );
        set_setting_value(&mut settings, "formatters.rust", json!("rustfmt"));
        assert_eq!(
            setting_value(&settings, "formatters.rust"),
            Some(&json!("rustfmt"))
        );
        remove_setting_value(&mut settings, "formatters.go");
        assert!(setting_value(&settings, "formatters.go").is_none());
        let path =
            std::env::temp_dir().join(format!("mtide-missing-settings-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let defaults = load_settings(&path).expect("default settings");
        assert!(setting_value(&defaults, "editor.tabSize").is_some());
    }

    #[test]
    fn settings_module_uses_explicit_config_path_and_key_scope() {
        let root =
            std::env::temp_dir().join(format!("mtide-settings-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        let config = root.join("settings.json");
        fs::write(&config, r#"{"formatters":{"go":"old"}}"#).expect("settings");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"settings-test","entry":"init.lua","api":2,"permissions":{"settings":true,"settings_keys":["formatters.*"]}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local settings = require("ttt.settings")
assert(settings.get("formatters.go") == "old")
settings.set("formatters.go", "gofmt")
assert(settings.get("formatters.go") == "gofmt")
local ok = pcall(function() settings.set("lsp.server", "blocked") end)
assert(ok == false)"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        PluginRuntime::load_with_settings_path(&package, Some(&config)).expect("runtime");
        let saved: JsonValue =
            serde_json::from_slice(&fs::read(&config).expect("saved settings")).expect("json");
        assert_eq!(saved["formatters"]["go"], "gofmt");
        fs::remove_dir_all(root).expect("cleanup");
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
    fn editor_context_menu_registers_and_invokes_items() {
        let root =
            std::env::temp_dir().join(format!("mtide-context-menu-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"context-menu","entry":"init.lua","api":2,"permissions":{"editor.read":true}}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local editor = require("ttt.editor")
editor.register_context_menu(function(line, col, word)
  assert(line == 1 and col == 2 and word == "hello")
  return {
    {label = "Log word", on_select = function()
      require("ttt").log("context:" .. word)
    end},
    {separator = true},
  }
end)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert_eq!(
            runtime.editor_context_menu(0, 1, "hello").expect("menu"),
            vec!["Log word", "────────"]
        );
        assert!(runtime.activate_editor_context_item(0).expect("activate"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] context:hello"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn editor_context_menu_requires_read_permission() {
        let root = std::env::temp_dir().join(format!(
            "mtide-context-menu-permission-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"restricted-context-menu","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local editor = require("ttt.editor")
local ok, err = pcall(function()
  editor.register_context_menu(function() return {} end)
end)
assert(not ok and tostring(err):find("permission denied: editor.read", 1, true) ~= nil)
"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        assert!(runtime
            .editor_context_menu(0, 0, "")
            .expect("empty menu")
            .is_empty());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn storage_module_persists_json_values_and_honors_permission() {
        let root =
            std::env::temp_dir().join(format!("mtide-storage-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        let name = format!("storage-test-{}", std::process::id());
        fs::write(
            root.join("plugin.ttt.json"),
            format!(
                r#"{{"name":"{name}","entry":"init.lua","api":2,"permissions":{{"storage":true}}}}"#
            ),
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local storage = require("ttt.storage")
storage.set("answer", 42)
storage.set("nested", {enabled = true, label = "demo"})
assert(storage.get("answer") == 42)
assert(storage.get("nested").enabled == true)
local keys = storage.keys()
assert(keys[1] == "answer" and keys[2] == "nested")
storage.remove("answer")
assert(storage.get("answer") == nil)"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        let runtime = PluginRuntime::load(&package).expect("runtime");
        drop(runtime);
        let storage_path = std::env::current_dir()
            .expect("current directory")
            .join(".mtide/plugin-storage")
            .join(format!("{name}.json"));
        let stored: JsonValue =
            serde_json::from_slice(&fs::read(&storage_path).expect("storage file"))
                .expect("stored json");
        assert_eq!(stored["nested"]["label"], "demo");
        fs::remove_file(storage_path).expect("storage cleanup");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn regex_module_matches_captures_and_reports_compile_errors() {
        let root = std::env::temp_dir().join(format!("mtide-regex-plugin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("directory");
        fs::write(
            root.join("plugin.ttt.json"),
            r#"{"name":"regex-test","entry":"init.lua","api":2}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("init.lua"),
            r#"local regex = require("ttt.regex")
local pattern = assert(regex.compile("(ab)(c)?", {ignore_case = true}))
local s, e, caps = pattern:find("xxAB", 2)
assert(s == 3 and e == 4 and caps[1] == "AB" and caps[2] == false)
local all = pattern:find_all("abc AB")
assert(#all == 2 and all[1].caps[1] == "ab" and all[2].caps[1] == "AB")
local invalid, err = regex.compile("[")
assert(invalid == nil and type(err) == "string")"#,
        )
        .expect("entry");
        let package = PluginPackage::load(&root).expect("package");
        PluginRuntime::load(&package).expect("runtime");
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
assert(type(crypto.random_bytes) == "function")
assert(#crypto.random_bytes(8) == 16)
assert(crypto.base64_decode(" YWJj\n") == "abc")
assert(type(crypto.uuid()) == "string")
assert(io == nil and debug == nil and os.execute == nil)
assert(type(os.time()) == "number" and type(os.clock()) == "number")
assert(type(os.date("%Y-%m-%d")) == "string")
assert(os.date("%p") == "AM" or os.date("%p") == "PM")
assert(os.date("%Z") == "UTC" and #os.date("%I") == 2)
assert(package.path == "" and package.cpath == "")
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
local limited_result = system.exec("/bin/sh", {"-c", "printf 123456"}, {max_output_bytes = 3})
assert(limited_result.truncated == true and limited_result.exit_code == -1 and limited_result.ok == false)
assert(#limited_result.stdout + #limited_result.stderr <= 3)
local async_handle = system.exec_async("/bin/echo", {"async"}, function(result)
  ttt.log("async:" .. result.stdout)
end)
assert(async_handle:id() > 0)
local streamed_handle = system.exec_async("/bin/sh", {"-c", "printf out; printf err >&2"}, {on_stdout = function(chunk) ttt.log("stdout:" .. chunk) end, on_stderr = function(chunk) ttt.log("stderr:" .. chunk) end}, function(result)
  assert(result.ok == true)
  ttt.log("streamed-done")
end)
assert(streamed_handle:id() > 0)
local cancel_handle = system.exec_async("/bin/sleep", {"2"}, function(result)
  assert(result.cancelled == true and result.timed_out == false)
  ttt.log("async-cancelled")
end)
assert(cancel_handle:cancel() == true)
fs.write("cache.txt", "ok")
assert(fs.read("cache.txt") == "ok")
local binary = string.char(0, 255, 128)
assert(fs.write("binary.dat", binary) == true)
assert(fs.read("binary.dat") == binary)
assert(fs.write("cache.txt", "ok") == true)
local missing, missing_err = fs.read("missing.txt")
assert(missing == nil and type(missing_err) == "string")
assert(fs.exists("cache.txt") == true)
assert(fs.exists("missing.txt") == false)
local listed = fs.list(".")
assert(type(listed) == "table" and listed[1] ~= nil and type(listed[1].name) == "string" and type(listed[1].is_dir) == "boolean")
local missing_list, missing_list_err = fs.list("missing-dir")
assert(missing_list == nil and type(missing_list_err) == "string")
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
ttt.register({sidebar = {title = "Demo", render = function(panel) panel:label("panel text"); panel:list({items = {"one", "two"}, selected = 2}); panel:checkbox({checked = true, label = "Check"}); panel:table({rows = {{cells = {"row", "value"}, right = "R"}}}); panel:tabs({items = {"A", "B"}, selected = 2}); panel:pager({page = 2, pages = 4}); panel:scrollview({items = {"scroll"}, selected = 1}); panel:dialog() end}, bottom = {id = "demo.bottom", title = "Bottom", render = function(panel) panel:label("bottom text") end}, commands = {{id = "demo.run", title = "Run", handler = function() local commands = ttt.list_commands(); assert(#commands == 2); assert(ttt.exec_command("demo.other") == true); assert(ttt.show_panel("sidebar") == true); assert(ttt.focus_panel("bottom") == true); assert(ttt.open_drawer({side = "left", width = 32, min_width = 12, render = function(panel) panel:label("drawer text") end}) == true); assert(ttt.open_tab({id = "demo-tab", title = "Demo Tab", focus = true, render = function(panel) panel:label("tab text") end, on_close = function() ttt.log("tab-closed") end}) == "demo-tab"); assert(ttt.focus_tab("demo-tab") == true); ttt.set_status_item("right", "live", "Live", {priority = 10}); ttt.remove_status_item("demo.status"); ttt.log("ran") end}, {id = "demo.other", title = "Other", handler = function() assert(ttt.open_readonly("Read-only", {"one", "two"}, "note.txt") == "Read-only"); assert(ttt.open_diff("Diff", {"old"}, {"new"}, "note.txt") == "Diff"); assert(ttt.open_file("src/main.rs", 12, true) == true); assert(ttt.clipboard_write("plugin clipboard") == true); assert(ttt.clipboard_read() == "plugin clipboard"); ttt.log("other") end}}, keybindings = {{key = "ctrl-r", command = "demo.run"}}, status_items = {{id = "demo.status", text = "Demo"}}, file_handlers = {{id = "demo.pdf", extensions = {"pdf"}, open = function(path, tab) assert(tab.id == "demo.pdf"); assert(tab.title == "demo.pdf"); tab:focus(); tab:set_title("PDF"); tab.on_close = function() ttt.log("closed") end return "view:" .. path end}, {id = "demo.view", extensions = {"view"}, open = function(_path, tab) tab.render = function(panel) panel:label("file view"); panel:list({items = {"first", "second"}, selected = 2}); panel:pager({page = 1, total = 3}) end end}}})"#,
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
            Some("panel text\n  one\n▶ two\n[x] Check\n  row  value  R\n  A\n▶ B\nPage 2/4\n▶ scroll".into())
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
            Some("file view\n  first\n▶ second\n« ‹ 1–1 of 3 › »".into())
        );
        assert_eq!(runtime.logs()[0], "[info] ok\n");
        assert_eq!(runtime.logs()[1], "[info] hello");
        for _ in 0..50 {
            runtime.poll_async().expect("poll async");
            if runtime
                .logs()
                .iter()
                .any(|line| line == "[info] async:async\n")
                && runtime
                    .logs()
                    .iter()
                    .any(|line| line == "[info] async-cancelled")
                && runtime
                    .logs()
                    .iter()
                    .any(|line| line == "[info] stdout:out")
                && runtime
                    .logs()
                    .iter()
                    .any(|line| line == "[info] stderr:err")
                && runtime
                    .logs()
                    .iter()
                    .any(|line| line == "[info] streamed-done")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] async:async\n"));
        assert!(runtime
            .logs()
            .iter()
            .any(|line| line == "[info] async-cancelled"));
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
                PluginPanelRequest::FocusTerminal,
                PluginPanelRequest::FocusFileView,
                PluginPanelRequest::OpenTab {
                    id: "Read-only".to_owned(),
                    title: "Read-only".to_owned(),
                    file_path: Some("note.txt".to_owned()),
                    focus: true,
                },
                PluginPanelRequest::OpenTab {
                    id: "Diff".to_owned(),
                    title: "Diff".to_owned(),
                    file_path: Some("note.txt".to_owned()),
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
                    file_path: None,
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

    #[test]
    fn crypto_helpers_round_trip_utf8_and_format_secure_identifiers() {
        let encoded = base64_encode("你好, mtide".as_bytes());
        assert_eq!(encoded, "5L2g5aW9LCBtdGlkZQ==");
        assert_eq!(
            String::from_utf8(base64_decode(&encoded).expect("decode")).expect("utf8"),
            "你好, mtide"
        );
        assert_eq!(base64_decode("  YWJj\n").expect("unpadded decode"), b"abc");
        assert!(base64_decode("a===").is_err());
        assert_eq!(
            base64_decode("AP+A").expect("binary decode"),
            vec![0, 255, 128]
        );

        let bytes = hex_encode(&[0, 1, 15, 16, 255]);
        assert_eq!(bytes, "00010f10ff");
        let random = SystemRandom::new();
        let mut uuid_bytes = [0_u8; 16];
        random.fill(&mut uuid_bytes).expect("random");
        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
        let uuid = format_uuid(&uuid_bytes);
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert!(matches!(uuid.as_bytes()[19], b'8'..=b'b'));
    }
}
