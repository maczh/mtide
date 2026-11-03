//! Workspace persistence. All writes go through the atomic writer here.

use encoding_rs::GBK;
use flate2::{write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("workspace data is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace TOML serialization failed: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("workspace TOML parse failed: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("file loading was cancelled")]
    Cancelled,
    #[error("file exceeds the configured load limit")]
    FileTooLarge,
}

/// Reads a file in bounded chunks, checking cancellation between reads.
/// Callers can use this before constructing a Rope so a large open operation
/// remains interruptible and never allocates beyond `max_bytes`.
pub fn read_file_cancellable(
    path: &Path,
    max_bytes: u64,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, WorkspaceError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > max_bytes {
        return Err(WorkspaceError::FileTooLarge);
    }
    let mut file = fs::File::open(path)?;
    let mut output = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(WorkspaceError::Cancelled);
        }
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() as u64 > max_bytes {
            return Err(WorkspaceError::FileTooLarge);
        }
    }
    Ok(output)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionTab {
    pub path: Option<PathBuf>,
    pub text: String,
    pub cursor_line: usize,
    pub cursor_column: usize,
    #[serde(default)]
    pub folded_lines: Vec<usize>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub preview: bool,
    #[serde(default)]
    pub dirty: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub path: Option<PathBuf>,
    pub text: String,
    pub cursor_line: usize,
    pub cursor_column: usize,
    #[serde(default)]
    pub workspace_roots: Vec<PathBuf>,
    #[serde(default)]
    pub folded_lines: Vec<usize>,
    #[serde(default)]
    pub tabs: Vec<SessionTab>,
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub bookmarks: Vec<usize>,
    #[serde(default)]
    pub recent_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticReport {
    pub version: String,
    pub platform: String,
    pub terminal: String,
    pub config_files: Vec<String>,
    pub notes: Vec<String>,
}

pub fn export_diagnostics(path: &Path, report: &DiagnosticReport) -> Result<(), WorkspaceError> {
    let json = serde_json::to_vec_pretty(report)?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json)?;
    let compressed = encoder.finish()?;
    atomic_write(path, &compressed)
}

pub fn redacted_path(path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    format!("path#{:016x}", hasher.finish())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceRoots {
    roots: Vec<PathBuf>,
}

/// A portable `.mtide` workspace description.
///
/// Paths are stored relative to the workspace file whenever possible so the
/// file can be checked into a repository and reused on another machine.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceFile {
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub layout: BTreeMap<String, String>,
    #[serde(default)]
    pub task_config: Option<String>,
    #[serde(default)]
    pub debug_config: Option<String>,
}

impl WorkspaceFile {
    pub fn from_roots(path: &Path, roots: &[PathBuf]) -> Self {
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self {
            roots: roots
                .iter()
                .map(|root| relative_or_absolute(base, root))
                .collect(),
            ..Self::default()
        }
    }

    pub fn resolve_roots(&self, path: &Path) -> Vec<PathBuf> {
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        self.roots
            .iter()
            .map(|root| {
                let candidate = PathBuf::from(root);
                if candidate.is_absolute() {
                    candidate
                } else {
                    base.join(candidate)
                }
            })
            .collect()
    }

    pub fn save(&self, path: &Path) -> Result<(), WorkspaceError> {
        let text = toml::to_string_pretty(self)?;
        atomic_write(path, text.as_bytes())
    }

    pub fn load(path: &Path) -> Result<Self, WorkspaceError> {
        Ok(toml::from_str(&fs::read_to_string(path)?)?)
    }
}

fn relative_or_absolute(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

impl WorkspaceRoots {
    pub fn new<I>(roots: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let mut workspace = Self::default();
        for root in roots {
            workspace.add(root)?;
        }
        Ok(workspace)
    }
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
    pub fn add(&mut self, root: PathBuf) -> io::Result<bool> {
        let canonical = match fs::canonicalize(&root) {
            Ok(path) => path,
            Err(_) if root.is_absolute() => root.clone(),
            Err(_) => std::env::current_dir()?.join(root),
        };
        if !canonical.is_dir() {
            return Ok(false);
        }
        if self.roots.iter().any(|existing| existing == &canonical) {
            return Ok(false);
        }
        self.roots.push(canonical);
        self.roots.sort();
        Ok(true)
    }
    pub fn remove(&mut self, root: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(root) else {
            return false;
        };
        let old_len = self.roots.len();
        self.roots.retain(|existing| existing != &canonical);
        old_len != self.roots.len()
    }
    pub fn roots_for(&self, path: &Path) -> Vec<&Path> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(root))
            .map(PathBuf::as_path)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEnding {
    Lf,
    Crlf,
    Cr,
    Mixed,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Latin1,
    Gbk,
    Binary,
}

pub fn detect_encoding(bytes: &[u8]) -> TextEncoding {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return TextEncoding::Utf8Bom;
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return TextEncoding::Utf16Le;
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return TextEncoding::Utf16Be;
    }
    if bytes.contains(&0) {
        TextEncoding::Binary
    } else if std::str::from_utf8(bytes).is_err() {
        let (_, _, had_errors) = GBK.decode(bytes);
        if !had_errors {
            TextEncoding::Gbk
        } else {
            TextEncoding::Latin1
        }
    } else {
        TextEncoding::Utf8
    }
}

pub fn decode_text(bytes: &[u8]) -> Option<(TextEncoding, String)> {
    match detect_encoding(bytes) {
        TextEncoding::Utf8 => String::from_utf8(bytes.to_vec())
            .ok()
            .map(|text| (TextEncoding::Utf8, text)),
        TextEncoding::Utf8Bom => String::from_utf8(bytes[3..].to_vec())
            .ok()
            .map(|text| (TextEncoding::Utf8Bom, text)),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let encoding = detect_encoding(bytes);
            let body = &bytes[2..];
            if body.len() % 2 != 0 {
                return None;
            }
            let units = body.chunks_exact(2).map(|pair| {
                if matches!(encoding, TextEncoding::Utf16Le) {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            });
            String::from_utf16(&units.collect::<Vec<_>>())
                .ok()
                .map(|text| (encoding, text))
        }
        TextEncoding::Latin1 => Some((
            TextEncoding::Latin1,
            bytes.iter().map(|byte| char::from(*byte)).collect(),
        )),
        TextEncoding::Gbk => {
            let (text, _, had_errors) = GBK.decode(bytes);
            (!had_errors).then_some((TextEncoding::Gbk, text.into_owned()))
        }
        TextEncoding::Binary => None,
    }
}

pub fn encode_text(text: &str, encoding: TextEncoding) -> Option<Vec<u8>> {
    match encoding {
        TextEncoding::Utf8 => Some(text.as_bytes().to_vec()),
        TextEncoding::Utf8Bom => Some([vec![0xef, 0xbb, 0xbf], text.as_bytes().to_vec()].concat()),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let mut bytes = if matches!(encoding, TextEncoding::Utf16Le) {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in text.encode_utf16() {
                bytes.extend(if matches!(encoding, TextEncoding::Utf16Le) {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            Some(bytes)
        }
        TextEncoding::Latin1 => text
            .chars()
            .map(|character| u8::try_from(character as u32).ok())
            .collect(),
        TextEncoding::Gbk => {
            let (bytes, _, had_errors) = GBK.encode(text);
            (!had_errors).then_some(bytes.into_owned())
        }
        TextEncoding::Binary => None,
    }
}

pub fn detect_line_ending(text: &str) -> LineEnding {
    let mut lf = false;
    let mut crlf = false;
    let mut cr = false;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf = true;
                index += 2;
            }
            b'\r' => {
                cr = true;
                index += 1;
            }
            b'\n' => {
                lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    match (lf, crlf, cr) {
        (false, false, false) => LineEnding::None,
        (true, false, false) => LineEnding::Lf,
        (false, true, false) => LineEnding::Crlf,
        (false, false, true) => LineEnding::Cr,
        _ => LineEnding::Mixed,
    }
}

pub fn normalize_line_endings(text: &str, ending: LineEnding) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match ending {
        LineEnding::Crlf => normalized.replace('\n', "\r\n"),
        LineEnding::Cr => normalized.replace('\n', "\r"),
        LineEnding::Lf => normalized,
        LineEnding::Mixed | LineEnding::None => text.to_owned(),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Config {
    pub tab_width: u8,
    pub insert_spaces: bool,
    pub word_wrap: bool,
    pub theme: String,
    pub keymap: String,
    pub scrollbar_mode: String,
    pub native_colors: bool,
    pub transparent_background: bool,
    pub file_icons: bool,
    pub terminal_scrollback: usize,
    pub terminal_force_keys: Vec<String>,
    /// Zero disables periodic autosave; named buffers use their file and
    /// unnamed buffers use the session snapshot.
    pub autosave_interval_secs: u64,
    pub save_all_on_quit: bool,
}

/// Staged settings used by WS-06. The caller applies the preview to its live
/// app state, but the user-level TOML is written only after `apply`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsTransaction {
    original: Config,
    draft: Config,
}

impl SettingsTransaction {
    pub fn new(config: Config) -> Self {
        Self {
            original: config.clone(),
            draft: config,
        }
    }
    pub fn draft(&self) -> &Config {
        &self.draft
    }
    pub fn draft_mut(&mut self) -> &mut Config {
        &mut self.draft
    }
    pub fn is_dirty(&self) -> bool {
        self.original != self.draft
    }
    pub fn cancel(self) -> Config {
        self.original
    }
    pub fn apply(self, path: &Path) -> Result<Config, WorkspaceError> {
        let text = toml::to_string_pretty(&self.draft)?;
        atomic_write(path, text.as_bytes())?;
        Ok(self.draft)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditorConfig {
    pub tab_width: Option<u8>,
    pub insert_spaces: Option<bool>,
    pub trim_trailing_whitespace: bool,
    pub insert_final_newline: bool,
}

/// Infers indentation from non-empty lines when no `.editorconfig` applies.
/// The most common positive indentation width wins; tabs take precedence when
/// they are observed on indented lines.
pub fn infer_indentation(text: &str) -> Option<(bool, u8)> {
    let mut tabs = 0usize;
    let mut spaces = BTreeMap::<usize, usize>::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let prefix = &line[..line.len() - line.trim_start().len()];
        if prefix.contains('\t') {
            tabs += 1;
        } else if !prefix.is_empty() {
            *spaces.entry(prefix.chars().count()).or_default() += 1;
        }
    }
    if tabs > 0 && tabs >= spaces.values().sum() {
        return Some((false, 1));
    }
    spaces
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .max_by_key(|(width, count)| (*count, std::cmp::Reverse(*width)))
        .and_then(|(width, _)| u8::try_from(width).ok().filter(|width| *width > 0))
        .map(|width| (true, width))
}

/// Reads the project-local `[*]` editorconfig section without making it a
/// startup dependency. Unknown keys and malformed values are ignored.
pub fn load_editorconfig(path: &Path) -> EditorConfig {
    let mut current = EditorConfig::default();
    let Ok(text) = fs::read_to_string(path) else {
        return current;
    };
    for line in text.lines().map(str::trim) {
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || line.starts_with('[')
        {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_ascii_lowercase();
        match key.trim().to_ascii_lowercase().as_str() {
            "indent_size" | "tab_width" => {
                current.tab_width = value.parse::<u8>().ok().filter(|v| *v > 0)
            }
            "indent_style" => current.insert_spaces = Some(value == "space"),
            "trim_trailing_whitespace" => current.trim_trailing_whitespace = value == "true",
            "insert_final_newline" => current.insert_final_newline = value == "true",
            _ => {}
        }
    }
    current
}

#[derive(Debug, Deserialize)]
struct ConfigLayer {
    tab_width: Option<u8>,
    insert_spaces: Option<bool>,
    word_wrap: Option<bool>,
    theme: Option<String>,
    keymap: Option<String>,
    scrollbar_mode: Option<String>,
    native_colors: Option<bool>,
    transparent_background: Option<bool>,
    file_icons: Option<bool>,
    terminal_scrollback: Option<usize>,
    terminal_force_keys: Option<Vec<String>>,
    autosave_interval_secs: Option<u64>,
    save_all_on_quit: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tab_width: 4,
            insert_spaces: true,
            word_wrap: true,
            theme: "retro".into(),
            keymap: "vscode".into(),
            scrollbar_mode: "thin".into(),
            native_colors: false,
            transparent_background: true,
            file_icons: false,
            terminal_scrollback: 1000,
            terminal_force_keys: vec!["ctrl-q".into(), "ctrl-`".into(), "ctrl-p".into()],
            autosave_interval_secs: 60,
            save_all_on_quit: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("configuration parse failed: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("configuration value is invalid: {0}")]
    InvalidValue(String),
}

/// Loads layers in increasing precedence; malformed optional layers are
/// returned to the caller so startup can report them without aborting.
pub fn load_config_layers(paths: &[&Path]) -> (Config, Vec<(PathBuf, ConfigError)>) {
    let mut config = Config::default();
    let mut errors = Vec::new();
    for path in paths {
        match fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<ConfigLayer>(&text) {
                Ok(mut layer) => {
                    if layer.tab_width == Some(0) {
                        errors.push((
                            (*path).to_path_buf(),
                            ConfigError::InvalidValue("tab_width must be greater than zero".into()),
                        ));
                        layer.tab_width = None;
                    }
                    merge_config(&mut config, layer);
                }
                Err(error) => errors.push(((*path).to_path_buf(), ConfigError::Parse(error))),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => errors.push(((*path).to_path_buf(), ConfigError::Io(error))),
        }
    }
    (config, errors)
}

fn merge_config(target: &mut Config, layer: ConfigLayer) {
    if let Some(value) = layer.tab_width {
        target.tab_width = value;
    }
    if let Some(value) = layer.insert_spaces {
        target.insert_spaces = value;
    }
    if let Some(value) = layer.word_wrap {
        target.word_wrap = value;
    }
    if let Some(value) = layer.theme {
        target.theme = value;
    }
    if let Some(value) = layer.keymap {
        if matches!(
            value.to_ascii_lowercase().as_str(),
            "vim" | "emacs" | "vscode" | "borland"
        ) {
            target.keymap = value;
        }
    }
    if let Some(value) = layer.scrollbar_mode {
        if matches!(value.as_str(), "thin" | "block" | "hidden") {
            target.scrollbar_mode = value;
        }
    }
    if let Some(value) = layer.native_colors {
        target.native_colors = value;
    }
    if let Some(value) = layer.transparent_background {
        target.transparent_background = value;
    }
    if let Some(value) = layer.file_icons {
        target.file_icons = value;
    }
    if let Some(value) = layer.terminal_scrollback {
        target.terminal_scrollback = value.clamp(1, 100_000);
    }
    if let Some(value) = layer.terminal_force_keys {
        target.terminal_force_keys = value;
    }
    if let Some(value) = layer.autosave_interval_secs {
        target.autosave_interval_secs = value.min(86_400);
    }
    if let Some(value) = layer.save_all_on_quit {
        target.save_all_on_quit = value;
    }
}

/// Write a file in the same directory, flush it, sync it, then rename it.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".{}.{stamp}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("mtide")
    ));
    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok::<(), io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(WorkspaceError::Io)
}

pub fn save_session(path: &Path, snapshot: &SessionSnapshot) -> Result<(), WorkspaceError> {
    atomic_write(path, &serde_json::to_vec_pretty(snapshot)?)
}

pub fn load_session(path: &Path) -> Result<SessionSnapshot, WorkspaceError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileFingerprint {
    pub length: u64,
    pub modified: Option<SystemTime>,
    pub hash: u64,
}

pub fn fingerprint(path: &Path) -> Result<FileFingerprint, WorkspaceError> {
    use std::hash::{Hash, Hasher};
    let metadata = fs::metadata(path)?;
    let bytes = fs::read(path)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(FileFingerprint {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        hash: hasher.finish(),
    })
}

pub fn changed_since(path: &Path, baseline: &FileFingerprint) -> Result<bool, WorkspaceError> {
    Ok(fingerprint(path)? != *baseline)
}

#[derive(Debug)]
pub struct WorkspaceWatcher {
    root: PathBuf,
    known: BTreeMap<PathBuf, (u64, Option<SystemTime>)>,
    max_files: usize,
    degraded: bool,
}

type FileMap = BTreeMap<PathBuf, (u64, Option<SystemTime>)>;

impl WorkspaceWatcher {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        Self::with_limit(root, 10_000)
    }

    pub fn with_limit(root: impl Into<PathBuf>, max_files: usize) -> Result<Self, WorkspaceError> {
        let root = root.into();
        let max_files = max_files.max(1);
        let mut watcher = Self {
            root,
            known: BTreeMap::new(),
            max_files,
            degraded: false,
        };
        let (known, degraded) = collect_files(&watcher.root, watcher.max_files)?;
        watcher.known = known;
        watcher.degraded = degraded;
        Ok(watcher)
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Polling is an intentional portable fallback; callers should debounce
    /// this method at the event-loop boundary (the target policy is 100 ms).
    pub fn poll(&mut self) -> Result<Vec<FileChange>, WorkspaceError> {
        let (current, degraded) = collect_files(&self.root, self.max_files)?;
        self.degraded = degraded;
        let mut changes = Vec::new();
        for (path, stamp) in &current {
            match self.known.get(path) {
                None => changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Created,
                }),
                Some(previous) if previous != stamp => changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                }),
                _ => {}
            }
        }
        for path in self.known.keys() {
            if !current.contains_key(path) {
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                });
            }
        }
        self.known = current;
        Ok(changes)
    }
}

fn collect_files(root: &Path, max_files: usize) -> Result<(FileMap, bool), WorkspaceError> {
    let mut files = BTreeMap::new();
    if !root.exists() {
        return Ok((files, false));
    }
    let truncated = collect_files_inner(root, &mut files, max_files)?;
    Ok((files, truncated))
}

fn collect_files_inner(
    path: &Path,
    files: &mut FileMap,
    max_files: usize,
) -> Result<bool, WorkspaceError> {
    if files.len() >= max_files {
        return Ok(true);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == ".git" || name == ".mtide")
        {
            return Ok(false);
        }
        for entry in fs::read_dir(path)? {
            if collect_files_inner(&entry?.path(), files, max_files)? {
                return Ok(true);
            }
        }
    } else if metadata.is_file() {
        files.insert(
            path.to_path_buf(),
            (metadata.len(), metadata.modified().ok()),
        );
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fr_ed11_atomic_write_round_trip() {
        let root = std::env::temp_dir().join(format!("mtide-test-{}", std::process::id()));
        let target = root.join("nested").join("file.txt");
        atomic_write(&target, "稳定内容🙂".as_bytes()).expect("atomic write");
        assert_eq!(
            fs::read_to_string(&target).expect("read result"),
            "稳定内容🙂"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws03_session_json_round_trip() {
        let root = std::env::temp_dir().join(format!("mtide-session-{}", std::process::id()));
        let target = root.join("session.json");
        let expected = SessionSnapshot {
            path: Some(PathBuf::from("main.rs")),
            text: "未保存".into(),
            cursor_line: 2,
            cursor_column: 4,
            workspace_roots: vec![PathBuf::from("workspace")],
            folded_lines: vec![1, 8],
            tabs: Vec::new(),
            active_tab: 0,
            bookmarks: vec![2],
            recent_files: vec![PathBuf::from("recent.rs")],
        };
        save_session(&target, &expected).expect("save session");
        assert_eq!(load_session(&target).expect("load session"), expected);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws09_workspace_file_round_trips_relative_roots_and_layout() {
        let root =
            std::env::temp_dir().join(format!("mtide-workspace-file-{}", std::process::id()));
        fs::create_dir_all(root.join("app")).expect("create app root");
        fs::create_dir_all(root.join("infra")).expect("create infra root");
        let file = root.join("project.mtide");
        let mut workspace =
            WorkspaceFile::from_roots(&file, &[root.join("app"), root.join("infra")]);
        workspace.layout.insert("sidebar.width".into(), "28".into());
        workspace.task_config = Some(".mtide/tasks.toml".into());
        workspace.save(&file).expect("save workspace file");

        let loaded = WorkspaceFile::load(&file).expect("load workspace file");
        assert_eq!(loaded.roots, vec!["app", "infra"]);
        assert_eq!(
            loaded.resolve_roots(&file),
            vec![root.join("app"), root.join("infra")]
        );
        assert_eq!(loaded.layout.get("sidebar.width"), Some(&"28".into()));
        assert_eq!(loaded.task_config.as_deref(), Some(".mtide/tasks.toml"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws02_project_config_overrides_defaults_without_blocking_on_invalid_config() {
        let root = std::env::temp_dir().join(format!("mtide-config-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create config fixture");
        let valid = root.join("valid.toml");
        let invalid = root.join("invalid.toml");
        let partial = root.join("partial.toml");
        let invalid_value = root.join("invalid-value.toml");
        fs::write(
            &valid,
            "tab_width = 2\nword_wrap = false\ntheme = 'modern'\n",
        )
        .expect("write config");
        fs::write(&invalid, "tab_width = [broken").expect("write invalid config");
        fs::write(&partial, "theme = 'retro'").expect("write partial config");
        fs::write(&invalid_value, "tab_width = 0\ntheme = 'modern'")
            .expect("write invalid value config");
        let (config, errors) = load_config_layers(&[&valid, &invalid]);
        assert_eq!(config.tab_width, 2);
        assert!(!config.word_wrap);
        assert_eq!(config.theme, "modern");
        assert_eq!(config.terminal_scrollback, 1000);
        assert_eq!(
            config.terminal_force_keys,
            vec!["ctrl-q", "ctrl-`", "ctrl-p"]
        );
        assert_eq!(config.autosave_interval_secs, 60);
        assert!(!config.save_all_on_quit);
        assert_eq!(errors.len(), 1);
        let (layered, errors) = load_config_layers(&[&valid, &partial]);
        assert!(errors.is_empty());
        assert_eq!(layered.tab_width, 2);
        assert!(!layered.word_wrap);
        assert_eq!(layered.theme, "retro");
        let (validated, errors) = load_config_layers(&[&valid, &invalid_value]);
        assert_eq!(validated.tab_width, 2);
        assert_eq!(validated.theme, "modern");
        assert_eq!(errors.len(), 1);
        let terminal = root.join("terminal.toml");
        fs::write(
            &terminal,
            "terminal_scrollback = 5000\nterminal_force_keys = ['ctrl-q', 'alt-x']\nautosave_interval_secs = 12\nsave_all_on_quit = true\nscrollbar_mode = 'block'\nnative_colors = true\ntransparent_background = false\nfile_icons = true\nkeymap = 'emacs'\n",
        )
        .expect("write terminal config");
        let (configured, errors) = load_config_layers(&[&terminal]);
        assert!(errors.is_empty());
        assert_eq!(configured.terminal_scrollback, 5000);
        assert_eq!(configured.scrollbar_mode, "block");
        assert!(configured.native_colors);
        assert!(!configured.transparent_background);
        assert_eq!(configured.keymap, "emacs");
        assert!(configured.file_icons);
        assert_eq!(configured.terminal_force_keys, vec!["ctrl-q", "alt-x"]);
        assert_eq!(configured.autosave_interval_secs, 12);
        assert!(configured.save_all_on_quit);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws06_settings_transaction_keeps_cancel_non_persistent_and_apply_atomic() {
        let root = std::env::temp_dir().join(format!("mtide-settings-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create settings fixture");
        let target = root.join("config.toml");
        let original = Config::default();
        let mut transaction = SettingsTransaction::new(original.clone());
        transaction.draft_mut().theme = "modern".into();
        assert!(transaction.is_dirty());
        assert_eq!(transaction.cancel(), original);
        let mut transaction = SettingsTransaction::new(Config::default());
        transaction.draft_mut().theme = "modern".into();
        let applied = transaction.apply(&target).expect("apply settings");
        assert_eq!(applied.theme, "modern");
        assert!(fs::read_to_string(&target)
            .expect("read settings")
            .contains("theme = \"modern\""));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws05_poller_reports_created_modified_and_deleted_files() {
        let root = std::env::temp_dir().join(format!("mtide-watch-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create watch fixture");
        let mut watcher = WorkspaceWatcher::new(&root).expect("create watcher");
        let file = root.join("a.txt");
        fs::create_dir_all(root.join(".git")).expect("create ignored git directory");
        fs::create_dir_all(root.join(".mtide")).expect("create ignored metadata directory");
        fs::write(&file, "one").expect("create file");
        assert_eq!(
            watcher.poll().expect("poll create")[0].kind,
            ChangeKind::Created
        );
        fs::write(root.join(".git").join("ignored"), "ignored").expect("write git file");
        fs::write(root.join(".mtide").join("ignored"), "ignored").expect("write metadata file");
        assert!(watcher.poll().expect("poll ignored files").is_empty());
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&file, "two").expect("modify file");
        assert_eq!(
            watcher.poll().expect("poll modify")[0].kind,
            ChangeKind::Modified
        );
        fs::remove_file(&file).expect("delete file");
        assert_eq!(
            watcher.poll().expect("poll delete")[0].kind,
            ChangeKind::Deleted
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ws05_watcher_degrades_when_file_limit_is_reached() {
        let root = std::env::temp_dir().join(format!("mtide-watch-limit-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create limit fixture");
        for index in 0..3 {
            fs::write(root.join(format!("{index}.txt")), "x").expect("write fixture");
        }
        let watcher = WorkspaceWatcher::with_limit(&root, 2).expect("create limited watcher");
        assert!(watcher.is_degraded());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed11_fingerprint_detects_external_content_change() {
        let root = std::env::temp_dir().join(format!("mtide-fingerprint-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create fingerprint fixture");
        let file = root.join("main.rs");
        fs::write(&file, "one").expect("write file");
        let baseline = fingerprint(&file).expect("fingerprint");
        assert!(!changed_since(&file, &baseline).expect("unchanged"));
        fs::write(&file, "two").expect("external change");
        assert!(changed_since(&file, &baseline).expect("changed"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed13_detects_and_preserves_line_endings() {
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Mixed);
        assert_eq!(
            normalize_line_endings("a\nb\n", LineEnding::Crlf),
            "a\r\nb\r\n"
        );
        let mixed = "a\r\nb\nc\r";
        assert_eq!(detect_line_ending(mixed), LineEnding::Mixed);
        assert_eq!(normalize_line_endings(mixed, LineEnding::Mixed), mixed);
    }

    #[test]
    fn fr_ws01_multi_root_add_remove_deduplicates_and_groups_paths() {
        let base = std::env::temp_dir().join(format!("mtide-roots-{}", std::process::id()));
        let first = base.join("one");
        let second = base.join("two");
        fs::create_dir_all(first.join("src")).expect("first root");
        fs::create_dir_all(&second).expect("second root");
        let mut roots = WorkspaceRoots::new([first.clone(), second.clone()]).expect("roots");
        assert_eq!(roots.roots().len(), 2);
        assert!(!roots.add(first.clone()).expect("dedup"));
        assert_eq!(roots.roots_for(&first.join("src/main.rs")).len(), 1);
        assert!(roots.remove(&second));
        assert_eq!(roots.roots().len(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn fr_ws02_editorconfig_reads_indent_and_save_policies() {
        let file =
            std::env::temp_dir().join(format!("mtide-editorconfig-{}.ini", std::process::id()));
        fs::write(&file, "root = true\n[*]\nindent_style = tab\nindent_size = 2\ntrim_trailing_whitespace = true\ninsert_final_newline = true\nunknown = ignored\n").expect("write editorconfig");
        assert_eq!(
            load_editorconfig(&file),
            EditorConfig {
                tab_width: Some(2),
                insert_spaces: Some(false),
                trim_trailing_whitespace: true,
                insert_final_newline: true
            }
        );
        let _ = fs::remove_file(file);
    }

    #[test]
    fn fr_ed09_infers_spaces_and_tabs_from_existing_text() {
        assert_eq!(
            infer_indentation("fn main() {\n    value();\n    done();\n"),
            Some((true, 4))
        );
        assert_eq!(
            infer_indentation("fn main() {\n\tvalue();\n"),
            Some((false, 1))
        );
        assert_eq!(infer_indentation("plain text\n"), None);
    }

    #[test]
    fn fr_ws07_diagnostic_export_is_atomic_and_does_not_include_raw_paths() {
        let root = std::env::temp_dir().join(format!("mtide-diagnostic-{}", std::process::id()));
        let target = root.join("report.json.gz");
        let raw = root.to_string_lossy().to_string();
        let report = DiagnosticReport {
            version: "0.1".into(),
            platform: "test".into(),
            terminal: "ansi16".into(),
            config_files: vec![redacted_path(&root)],
            notes: vec!["sensitive configuration omitted".into()],
        };
        export_diagnostics(&target, &report).expect("export diagnostics");
        let compressed = fs::read(&target).expect("read diagnostics");
        let mut decoder = flate2::read::GzDecoder::new(compressed.as_slice());
        let mut output = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut output).expect("decompress diagnostics");
        assert!(!output.contains(&raw));
        assert!(output.contains("path#"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fr_ed13_detects_bom_and_decodes_utf16() {
        assert_eq!(
            detect_encoding(&[0xef, 0xbb, 0xbf, b'a']),
            TextEncoding::Utf8Bom
        );
        let bytes = [0xff, 0xfe, b'a', 0, 0x2d, 0x4e];
        assert_eq!(
            decode_text(&bytes),
            Some((TextEncoding::Utf16Le, "a中".into()))
        );
        assert_eq!(decode_text(&[0, 1, 2]), None);
        assert_eq!(
            decode_text(&encode_text("a中", TextEncoding::Utf16Be).expect("encode"))
                .map(|(_, text)| text),
            Some("a中".into())
        );
    }

    #[test]
    fn fr_ed13_invalid_utf8_without_nul_round_trips_as_latin1() {
        let bytes = [0x41, 0xE9, 0xFF];
        assert_eq!(detect_encoding(&bytes), TextEncoding::Latin1);
        let decoded = decode_text(&bytes).expect("latin1 decode");
        assert_eq!(decoded, (TextEncoding::Latin1, "Aéÿ".into()));
        assert_eq!(
            encode_text(&decoded.1, TextEncoding::Latin1),
            Some(bytes.to_vec())
        );
    }

    #[test]
    fn fr_ed13_detects_and_round_trips_gbk_before_latin1_fallback() {
        let original = "中文 GBK";
        let bytes = encode_text(original, TextEncoding::Gbk).expect("encode GBK");
        assert_eq!(detect_encoding(&bytes), TextEncoding::Gbk);
        assert_eq!(
            decode_text(&bytes),
            Some((TextEncoding::Gbk, original.into()))
        );
    }

    #[test]
    fn fr_ed12_cancellable_reader_stops_and_enforces_limit() {
        let root = std::env::temp_dir().join(format!("mtide-read-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create reader fixture");
        let file = root.join("large.txt");
        fs::write(&file, vec![b'x'; 128 * 1024]).expect("write reader fixture");
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            read_file_cancellable(&file, 256 * 1024, &cancelled),
            Err(WorkspaceError::Cancelled)
        ));
        let not_cancelled = AtomicBool::new(false);
        assert!(matches!(
            read_file_cancellable(&file, 64 * 1024, &not_cancelled),
            Err(WorkspaceError::FileTooLarge)
        ));
        let _ = fs::remove_dir_all(root);
    }
}
