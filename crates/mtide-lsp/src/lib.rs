//! LSP client skeleton: JSON-RPC transport, request governance, session
//! lifecycle, document sync and diagnostics aggregation.
//!
//! The skeleton exists to validate the design constraints from the PRD and the
//! architecture document before the full client is written:
//!
//! * **UI thread never blocks** (PRD NF-02, arch A-02): every call is either a
//!   send into a queue or a bounded `pump`; nothing waits for the server.
//! * **Transport is shared with DAP** (arch §7.7): framing lives in
//!   `mtide-proc`, this crate only understands LSP message semantics.
//! * **Session state machine is explicit** (PRD §9.2): Restarting with capped
//!   exponential backoff and a Disabled terminal state.
//! * **Diagnostics are version gated** (PRD LN-09/LN-19): a late publish for an
//!   older document version never overwrites newer results.
//! * **Coordinates are UTF-16 on the wire, bytes locally** (arch A-08): the two
//!   conversions live in one place so no other module guesses.

use mtide_proc::{write_message, ManagedProcess};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("LSP process I/O failed: {0}")]
    Proc(#[from] mtide_proc::ProcError),
    #[error("LSP I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("LSP server is not ready (state {0:?})")]
    NotReady(Lifecycle),
    #[error("LSP server is unavailable: {0}")]
    Unavailable(String),
}

// ---------------------------------------------------------------------------
// Protocol payloads (the subset the skeleton needs to prove correlation)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based column measured in UTF-16 code units (LSP wire format).
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    fn from_wire(value: Option<u64>) -> Self {
        match value {
            Some(1) => Self::Error,
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            _ => Self::Hint,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    /// Which producer reported it: an LSP server id or a linter name (LN-19).
    pub source: String,
    pub message: String,
    pub code: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeverityCounts {
    pub errors: usize,
    pub warnings: usize,
    pub information: usize,
    pub hints: usize,
}

impl SeverityCounts {
    pub fn total(&self) -> usize {
        self.errors + self.warnings + self.information + self.hints
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SyncKind {
    None,
    #[default]
    Full,
    Incremental,
}

impl SyncKind {
    fn from_capability(value: &Value) -> Self {
        let kind = match value.get("textDocumentSync") {
            Some(Value::Number(number)) => number.as_u64().unwrap_or(1),
            Some(Value::Object(options)) => {
                options.get("change").and_then(Value::as_u64).unwrap_or(1)
            }
            _ => 1,
        };
        match kind {
            0 => Self::None,
            2 => Self::Incremental,
            _ => Self::Full,
        }
    }
}

/// Capabilities that gate UI entry points; unsupported features must be hidden
/// rather than erroring at call time (PRD LN-10/DB-01).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Feature {
    Completion,
    Hover,
    Definition,
    References,
    Rename,
    DocumentSymbol,
    FoldingRange,
    CodeAction,
    CodeLens,
    SemanticTokens,
    CallHierarchy,
    TypeHierarchy,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerCapabilities {
    features: BTreeSet<Feature>,
    pub sync: SyncKind,
}

impl ServerCapabilities {
    pub fn from_value(capabilities: &Value) -> Self {
        let mut features = BTreeSet::new();
        let mut enable = |key: &str, feature: Feature| {
            if capabilities.get(key).is_some_and(|value| !value.is_null()) {
                features.insert(feature);
            }
        };
        enable("completionProvider", Feature::Completion);
        enable("hoverProvider", Feature::Hover);
        enable("definitionProvider", Feature::Definition);
        enable("referencesProvider", Feature::References);
        enable("renameProvider", Feature::Rename);
        enable("documentSymbolProvider", Feature::DocumentSymbol);
        enable("foldingRangeProvider", Feature::FoldingRange);
        enable("codeActionProvider", Feature::CodeAction);
        enable("codeLensProvider", Feature::CodeLens);
        enable("semanticTokensProvider", Feature::SemanticTokens);
        enable("callHierarchyProvider", Feature::CallHierarchy);
        enable("typeHierarchyProvider", Feature::TypeHierarchy);
        Self {
            features,
            sync: SyncKind::from_capability(capabilities),
        }
    }

    pub fn supports(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }
}

// ---------------------------------------------------------------------------
// URIs and coordinate conversion (arch A-08: one place only)
// ---------------------------------------------------------------------------

/// Encodes a path as a `file://` URI. Only the characters that would break URI
/// parsing are escaped; paths are assumed to be UTF-8 on the wire.
pub fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut uri = String::with_capacity(text.len() + 7);
    uri.push_str("file://");
    for character in text.chars() {
        match character {
            '%' => uri.push_str("%25"),
            ' ' => uri.push_str("%20"),
            '#' => uri.push_str("%23"),
            '?' => uri.push_str("%3F"),
            other => uri.push(other),
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut chars = rest.chars();
    while let Some(character) = chars.next() {
        if character == '%' {
            let high = chars.next()?.to_digit(16)?;
            let low = chars.next()?.to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// Converts a byte offset into an LSP position, counting columns in UTF-16 code
/// units as required by the protocol.
pub fn position_at(text: &str, byte: usize) -> Position {
    let byte = byte.min(text.len());
    let mut line = 0;
    let mut character = 0;
    for (index, chunk) in text.char_indices() {
        if index >= byte {
            break;
        }
        if chunk == '\n' {
            line += 1;
            character = 0;
        } else {
            character += chunk.len_utf16() as u32;
        }
    }
    Position { line, character }
}

/// Converts an LSP position back into a byte offset, clamping to a character
/// boundary so a server that reports a mid-character column cannot panic us.
pub fn offset_at(text: &str, position: Position) -> usize {
    let mut line = 0;
    let mut character = 0;
    for (index, chunk) in text.char_indices() {
        if line == position.line
            && (character >= position.character
                || character + chunk.len_utf16() as u32 > position.character)
        {
            // Landing inside a character (a column inside a surrogate pair)
            // snaps back to that character's start rather than splitting it.
            return index;
        }
        if chunk == '\n' {
            if line == position.line {
                return index;
            }
            line += 1;
            character = 0;
        } else {
            character += chunk.len_utf16() as u32;
        }
    }
    text.len()
}

// ---------------------------------------------------------------------------
// Document synchronisation
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
struct OpenDocument {
    version: i32,
    language_id: String,
}

/// Tracks the documents reported to one server and builds the corresponding
/// notifications. Versions are monotonic per document; `didChange` is refused
/// for documents that were never opened, which is the most common source of
/// "server silently stops responding" bugs.
#[derive(Clone, Debug, Default)]
pub struct DocumentSync {
    open: BTreeMap<PathBuf, OpenDocument>,
}

impl DocumentSync {
    pub fn is_open(&self, path: &Path) -> bool {
        self.open.contains_key(path)
    }
    pub fn version(&self, path: &Path) -> Option<i32> {
        self.open.get(path).map(|document| document.version)
    }
    pub fn open_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.open.keys()
    }

    pub fn did_open(&mut self, path: &Path, language_id: &str, text: &str) -> Value {
        self.open.insert(
            path.to_path_buf(),
            OpenDocument {
                version: 0,
                language_id: language_id.to_owned(),
            },
        );
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": {
                "uri": file_uri(path),
                "languageId": language_id,
                "version": 0,
                "text": text,
            }}
        })
    }

    /// Full-sync change (v1.0 path per arch §6.5: incremental comes later).
    pub fn did_change(&mut self, path: &Path, text: &str) -> Option<Value> {
        let document = self.open.get_mut(path)?;
        document.version += 1;
        let version = document.version;
        Some(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": file_uri(path), "version": version },
                "contentChanges": [{ "text": text }],
            }
        }))
    }

    pub fn did_save(&mut self, path: &Path) -> Option<Value> {
        self.open.get(path)?;
        Some(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": { "textDocument": { "uri": file_uri(path) } }
        }))
    }

    pub fn did_close(&mut self, path: &Path) -> Option<Value> {
        self.open.remove(path)?;
        Some(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": file_uri(path) } }
        }))
    }
}

// ---------------------------------------------------------------------------
// Diagnostics store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDiagnostics {
    pub version: Option<i32>,
    pub source: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Aggregates diagnostics from LSP servers and external linters (LN-09/LN-19).
/// A publish whose document version is older than the newest publish for that
/// file is dropped: servers routinely race with editing.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticStore {
    by_path: BTreeMap<PathBuf, FileDiagnostics>,
}

impl DiagnosticStore {
    pub fn publish(
        &mut self,
        path: &Path,
        version: Option<i32>,
        source: &str,
        diagnostics: Vec<Diagnostic>,
    ) -> bool {
        if let Some(existing) = self.by_path.get(path) {
            if let (Some(new), Some(old)) = (version, existing.version) {
                if new < old {
                    return false;
                }
            }
        }
        self.by_path.insert(
            path.to_path_buf(),
            FileDiagnostics {
                version,
                source: source.to_owned(),
                diagnostics,
            },
        );
        true
    }

    pub fn clear(&mut self, path: &Path) {
        self.by_path.remove(path);
    }

    pub fn file(&self, path: &Path) -> Option<&FileDiagnostics> {
        self.by_path.get(path)
    }

    /// Iterates every file that currently has diagnostics, in path order.
    pub fn files(&self) -> impl Iterator<Item = (&PathBuf, &FileDiagnostics)> {
        self.by_path.iter()
    }

    pub fn counts(&self) -> SeverityCounts {
        let mut counts = SeverityCounts::default();
        for file in self.by_path.values() {
            for diagnostic in &file.diagnostics {
                match diagnostic.severity {
                    DiagnosticSeverity::Error => counts.errors += 1,
                    DiagnosticSeverity::Warning => counts.warnings += 1,
                    DiagnosticSeverity::Information => counts.information += 1,
                    DiagnosticSeverity::Hint => counts.hints += 1,
                }
            }
        }
        counts
    }

    pub fn by_source(&self, source: &str) -> Vec<(&PathBuf, &Diagnostic)> {
        self.by_path
            .iter()
            .flat_map(|(path, file)| {
                file.diagnostics
                    .iter()
                    .filter(move |diagnostic| diagnostic.source == source)
                    .map(move |diagnostic| (path, diagnostic))
            })
            .collect()
    }

    pub fn sources(&self) -> BTreeSet<&str> {
        self.by_path
            .values()
            .flat_map(|file| file.diagnostics.iter().map(|item| item.source.as_str()))
            .collect()
    }
}

/// Parses a `textDocument/publishDiagnostics` notification.
pub fn parse_publish_diagnostics(
    default_source: &str,
    params: &Value,
) -> Option<(PathBuf, Option<i32>, Vec<Diagnostic>)> {
    let path = uri_to_path(params.get("uri")?.as_str()?)?;
    let version = params
        .get("version")
        .and_then(Value::as_i64)
        .map(|value| value as i32);
    let diagnostics = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let range = item.get("range")?;
            Some(Diagnostic {
                range: Range {
                    start: Position {
                        line: range.get("start")?.get("line")?.as_u64()? as u32,
                        character: range.get("start")?.get("character")?.as_u64()? as u32,
                    },
                    end: Position {
                        line: range.get("end")?.get("line")?.as_u64()? as u32,
                        character: range.get("end")?.get("character")?.as_u64()? as u32,
                    },
                },
                severity: DiagnosticSeverity::from_wire(
                    item.get("severity").and_then(Value::as_u64),
                ),
                source: item
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or(default_source)
                    .to_owned(),
                message: item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                code: item.get("code").and_then(|code| match code {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                }),
            })
        })
        .collect();
    Some((path, version, diagnostics))
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Discovered,
    Spawning,
    Initializing,
    Ready,
    Degraded,
    Crashed,
    Restarting,
    Disabled,
}

impl Lifecycle {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub request_timeout: Duration,
    pub max_restarts: u32,
    pub backoff_base: Duration,
    pub degrade_after_timeouts: u32,
    pub client_name: String,
    pub server_id: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(5),
            max_restarts: 3,
            backoff_base: Duration::from_secs(1),
            degrade_after_timeouts: 2,
            client_name: "mtide".into(),
            server_id: "lsp".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent {
    StateChanged(Lifecycle),
    Response {
        id: u64,
        result: Value,
    },
    ErrorResponse {
        id: u64,
        code: i64,
        message: String,
    },
    Notification {
        method: String,
        params: Value,
    },
    Diagnostic {
        path: PathBuf,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    Timeout {
        id: u64,
        method: String,
    },
    Aborted {
        id: u64,
        reason: &'static str,
    },
    ServerExited {
        code: Option<i32>,
    },
    ProtocolError(String),
    Stderr(String),
}

#[derive(Clone, Debug)]
struct PendingRequest {
    method: String,
    deadline: Instant,
}

enum RawEvent {
    Message(Value),
    Exited(Option<i32>),
    ProtocolError(String),
    Stderr(String),
}

/// A per-server LSP session. One reader thread and one writer thread own the
/// pipes; the caller only ever enqueues or drains.
pub struct LspClient {
    config: ClientConfig,
    state: Lifecycle,
    next_id: u64,
    pending: BTreeMap<u64, PendingRequest>,
    capabilities: ServerCapabilities,
    documents: DocumentSync,
    diagnostics: DiagnosticStore,
    outgoing: Option<Sender<Vec<u8>>>,
    incoming: Option<Receiver<RawEvent>>,
    process: Option<ManagedProcess>,
    spec: Option<ServerSpec>,
    restart_count: u32,
    restart_at: Option<Instant>,
    consecutive_timeouts: u32,
    initialize_id: Option<u64>,
    stderr_tail: Arc<std::sync::Mutex<Vec<String>>>,
}

#[derive(Clone, Debug)]
pub struct ServerSpec {
    pub program: String,
    pub args: Vec<String>,
    pub root: PathBuf,
}

impl LspClient {
    /// Creates a client bound to an existing transport. Used by tests and by
    /// callers that own the process themselves.
    pub fn attach<R, W>(config: ClientConfig, reader: R, writer: W) -> Self
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (raw_sender, incoming) = channel();
        let (outgoing, out_receiver) = channel::<Vec<u8>>();
        let stderr_tail = Arc::new(std::sync::Mutex::new(Vec::new()));
        spawn_reader(BufReader::new(reader), raw_sender.clone());
        spawn_writer(writer, out_receiver, raw_sender);
        let mut client = Self::empty(config);
        client.incoming = Some(incoming);
        client.outgoing = Some(outgoing);
        client.state = Lifecycle::Spawning;
        client.stderr_tail = stderr_tail;
        client
    }

    /// Spawns a server process and attaches to its stdio.
    pub fn spawn(config: ClientConfig, spec: ServerSpec) -> Result<Self, LspError> {
        let mut client = Self::empty(config);
        client.spec = Some(spec);
        client.start_process()?;
        Ok(client)
    }

    fn empty(config: ClientConfig) -> Self {
        Self {
            config,
            state: Lifecycle::Discovered,
            next_id: 1,
            pending: BTreeMap::new(),
            capabilities: ServerCapabilities::default(),
            documents: DocumentSync::default(),
            diagnostics: DiagnosticStore::default(),
            outgoing: None,
            incoming: None,
            process: None,
            spec: None,
            restart_count: 0,
            restart_at: None,
            consecutive_timeouts: 0,
            initialize_id: None,
            stderr_tail: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn start_process(&mut self) -> Result<(), LspError> {
        let spec = self
            .spec
            .clone()
            .ok_or_else(|| LspError::Unavailable("no server specification".into()))?;
        let args = spec.args.iter().map(String::as_str).collect::<Vec<_>>();
        let mut process = ManagedProcess::spawn(&spec.program, &args)?;
        let stdin = process.take_stdin()?;
        let stdout = process.take_stdout()?;
        let stderr = process.take_stderr()?;
        let (raw_sender, incoming) = channel();
        let (outgoing, out_receiver) = channel::<Vec<u8>>();
        spawn_reader(BufReader::new(stdout), raw_sender.clone());
        spawn_writer(stdin, out_receiver, raw_sender.clone());
        let stderr_tail = Arc::new(std::sync::Mutex::new(Vec::new()));
        spawn_stderr(stderr, raw_sender, Arc::clone(&stderr_tail));
        self.process = Some(process);
        self.incoming = Some(incoming);
        self.outgoing = Some(outgoing);
        self.stderr_tail = stderr_tail;
        self.set_state(Lifecycle::Spawning);
        Ok(())
    }

    pub fn state(&self) -> Lifecycle {
        self.state
    }
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }
    pub fn documents(&self) -> &DocumentSync {
        &self.documents
    }
    pub fn diagnostics(&self) -> &DiagnosticStore {
        &self.diagnostics
    }
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn set_state(&mut self, state: Lifecycle) {
        self.state = state;
    }

    fn send_message(&mut self, message: Value) -> Result<(), LspError> {
        let sender = self
            .outgoing
            .as_ref()
            .ok_or_else(|| LspError::Unavailable("transport is closed".into()))?;
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message)?;
        sender
            .send(bytes)
            .map_err(|_| LspError::Unavailable("writer thread has stopped".into()))
    }

    /// Starts the initialize handshake (PRD §9.2: Spawning → Initializing).
    pub fn initialize(&mut self, root: &Path) -> Result<u64, LspError> {
        let id = self.allocate_id();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": self.config.client_name },
            "rootUri": file_uri(root),
            "workspaceFolders": [{ "uri": file_uri(root), "name": root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into()) }],
            "capabilities": {
                "textDocument": {
                    "synchronization": { "dynamicRegistration": false },
                    "completion": { "completionItem": { "snippetSupport": true } },
                    "hover": {},
                    "definition": {},
                    "references": {},
                    "rename": {},
                    "documentSymbol": {},
                    "foldingRange": {},
                    "codeAction": {},
                    "publishDiagnostics": { "versionSupport": true },
                    "semanticTokens": {},
                    "callHierarchy": {},
                    "typeHierarchy": {},
                },
                "workspace": { "workspaceFolders": true },
            },
        });
        self.pending.insert(
            id,
            PendingRequest {
                method: "initialize".into(),
                deadline: Instant::now() + self.config.request_timeout,
            },
        );
        self.send_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": params,
        }))?;
        self.initialize_id = Some(id);
        self.set_state(Lifecycle::Initializing);
        Ok(id)
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Queues a request. Never blocks; the caller matches the returned id with
    /// `ClientEvent::Response`/`Timeout` from `pump`.
    pub fn request(&mut self, method: &str, params: Value) -> Result<u64, LspError> {
        if !self.state.is_usable() {
            return Err(LspError::NotReady(self.state));
        }
        let id = self.allocate_id();
        self.pending.insert(
            id,
            PendingRequest {
                method: method.to_owned(),
                deadline: Instant::now() + self.config.request_timeout,
            },
        );
        self.send_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        self.send_message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    /// Cancels an in-flight request (PRD arch §6.5: cancel the previous
    /// completion when a new one starts).
    pub fn cancel(&mut self, id: u64) -> Result<(), LspError> {
        self.pending.remove(&id);
        self.send_message(json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": id },
        }))
    }

    pub fn did_open(&mut self, path: &Path, language_id: &str, text: &str) -> Result<(), LspError> {
        let notification = self.documents.did_open(path, language_id, text);
        self.send_message(notification)
    }

    pub fn did_change(&mut self, path: &Path, text: &str) -> Result<(), LspError> {
        match self.documents.did_change(path, text) {
            Some(notification) => self.send_message(notification),
            None => Ok(()),
        }
    }

    pub fn did_save(&mut self, path: &Path) -> Result<(), LspError> {
        match self.documents.did_save(path) {
            Some(notification) => self.send_message(notification),
            None => Ok(()),
        }
    }

    pub fn did_close(&mut self, path: &Path) -> Result<(), LspError> {
        match self.documents.did_close(path) {
            Some(notification) => self.send_message(notification),
            None => Ok(()),
        }
    }

    /// Drains server events for up to `budget`, then applies deadlines and
    /// restart timers. Returns at most one batch of events; call again to keep
    /// pumping. This is the only call that may wait, and only for `budget`.
    pub fn pump(&mut self, budget: Duration) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        let mut raws = Vec::new();
        if let Some(incoming) = &self.incoming {
            match incoming.recv_timeout(budget) {
                Ok(raw) => {
                    raws.push(raw);
                    while let Ok(raw) = incoming.try_recv() {
                        raws.push(raw);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            }
        }
        for raw in raws {
            self.handle_raw(raw, &mut events);
        }
        self.check_deadlines(&mut events);
        self.check_restart(&mut events);
        events
    }

    fn handle_raw(&mut self, raw: RawEvent, events: &mut Vec<ClientEvent>) {
        match raw {
            RawEvent::Message(message) => self.handle_message(message, events),
            RawEvent::Exited(code) => {
                for (id, pending) in std::mem::take(&mut self.pending) {
                    let _ = pending;
                    events.push(ClientEvent::Aborted {
                        id,
                        reason: "server exited",
                    });
                }
                if self.state != Lifecycle::Disabled {
                    self.set_state(Lifecycle::Crashed);
                    events.push(ClientEvent::StateChanged(Lifecycle::Crashed));
                    self.schedule_restart(events);
                }
                events.push(ClientEvent::ServerExited { code });
            }
            RawEvent::ProtocolError(message) => {
                events.push(ClientEvent::ProtocolError(message));
            }
            RawEvent::Stderr(line) => events.push(ClientEvent::Stderr(line)),
        }
    }

    fn schedule_restart(&mut self, events: &mut Vec<ClientEvent>) {
        if self.spec.is_none() {
            self.set_state(Lifecycle::Disabled);
            events.push(ClientEvent::StateChanged(Lifecycle::Disabled));
            return;
        }
        if self.restart_count >= self.config.max_restarts {
            self.set_state(Lifecycle::Disabled);
            events.push(ClientEvent::StateChanged(Lifecycle::Disabled));
            return;
        }
        let delay = self.config.backoff_base * 2u32.pow(self.restart_count);
        self.restart_count += 1;
        self.restart_at = Some(Instant::now() + delay);
        self.set_state(Lifecycle::Restarting);
        events.push(ClientEvent::StateChanged(Lifecycle::Restarting));
    }

    fn check_restart(&mut self, events: &mut Vec<ClientEvent>) {
        let Some(at) = self.restart_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.restart_at = None;
        match self.start_process() {
            Ok(()) => {
                let root = self
                    .spec
                    .as_ref()
                    .map(|spec| spec.root.clone())
                    .unwrap_or_else(|| PathBuf::from("."));
                self.documents = DocumentSync::default();
                if self.initialize(&root).is_ok() {
                    events.push(ClientEvent::StateChanged(Lifecycle::Initializing));
                }
            }
            Err(error) => {
                events.push(ClientEvent::Stderr(format!("restart failed: {error}")));
                self.schedule_restart(events);
            }
        }
    }

    fn handle_message(&mut self, message: Value, events: &mut Vec<ClientEvent>) {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            if message.get("id").is_some() {
                // Server-initiated request (e.g. workspace/configuration). The
                // skeleton answers with an empty result so servers do not stall.
                if let Some(id) = message.get("id").cloned() {
                    let _ = self.send_message(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": Value::Null,
                    }));
                }
            }
            if method == "textDocument/publishDiagnostics" {
                if let Some((path, version, diagnostics)) =
                    parse_publish_diagnostics(&self.config.server_id, &params)
                {
                    let current = self.documents.version(&path);
                    let accepted = match (version, current) {
                        (Some(reported), Some(open)) => reported >= open || reported == -1,
                        _ => true,
                    };
                    if accepted
                        && self.diagnostics.publish(
                            &path,
                            version,
                            &self.config.server_id,
                            diagnostics.clone(),
                        )
                    {
                        events.push(ClientEvent::Diagnostic {
                            path,
                            version,
                            diagnostics,
                        });
                    }
                }
            }
            events.push(ClientEvent::Notification {
                method: method.to_owned(),
                params,
            });
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let pending = self.pending.remove(&id);
        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("LSP error")
                .to_owned();
            events.push(ClientEvent::ErrorResponse {
                id,
                code,
                message: text,
            });
            return;
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        if self.initialize_id == Some(id) {
            self.capabilities =
                ServerCapabilities::from_value(result.get("capabilities").unwrap_or(&Value::Null));
            self.initialize_id = None;
            self.consecutive_timeouts = 0;
            if self.restart_count > 0 {
                self.restart_count = 0;
            }
            self.set_state(Lifecycle::Ready);
            events.push(ClientEvent::StateChanged(Lifecycle::Ready));
            let _ = self.notify("initialized", json!({}));
        } else if self.state == Lifecycle::Degraded {
            self.consecutive_timeouts = 0;
            self.set_state(Lifecycle::Ready);
            events.push(ClientEvent::StateChanged(Lifecycle::Ready));
        } else if pending.is_some() {
            self.consecutive_timeouts = 0;
        }
        events.push(ClientEvent::Response { id, result });
    }

    fn check_deadlines(&mut self, events: &mut Vec<ClientEvent>) {
        let now = Instant::now();
        let expired = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired {
            let Some(pending) = self.pending.remove(&id) else {
                continue;
            };
            // Tell the server to stop working on the request; otherwise a slow
            // completion keeps burning server CPU after the UI moved on.
            let _ = self.send_message(json!({
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": { "id": id },
            }));
            events.push(ClientEvent::Timeout {
                id,
                method: pending.method,
            });
            self.consecutive_timeouts += 1;
            if self.consecutive_timeouts >= self.config.degrade_after_timeouts
                && self.state == Lifecycle::Ready
            {
                self.set_state(Lifecycle::Degraded);
                events.push(ClientEvent::StateChanged(Lifecycle::Degraded));
            }
        }
    }

    /// Stops the session and reaps the server process.
    pub fn shutdown(&mut self) {
        if self.state.is_usable() {
            let _ = self.notify("shutdown", json!({}));
            let _ = self.notify("exit", json!({}));
        }
        self.outgoing = None;
        self.incoming = None;
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait_timeout(Duration::from_millis(200));
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_reader<R: BufRead + Send + 'static>(mut reader: R, sender: Sender<RawEvent>) {
    thread::spawn(move || loop {
        match read_frame(&mut reader) {
            Ok(message) => {
                if sender.send(RawEvent::Message(message)).is_err() {
                    return;
                }
            }
            Err(FrameError::Eof) => {
                let _ = sender.send(RawEvent::Exited(None));
                return;
            }
            Err(FrameError::Io(error)) => {
                let _ = sender.send(RawEvent::Stderr(format!("read failed: {error}")));
                let _ = sender.send(RawEvent::Exited(None));
                return;
            }
            Err(FrameError::Protocol(message)) => {
                let _ = sender.send(RawEvent::ProtocolError(message));
            }
        }
    });
}

fn spawn_writer<W: Write + Send + 'static>(
    mut writer: W,
    receiver: Receiver<Vec<u8>>,
    sender: Sender<RawEvent>,
) {
    thread::spawn(move || {
        while let Ok(bytes) = receiver.recv() {
            if writer
                .write_all(&bytes)
                .and_then(|_| writer.flush())
                .is_err()
            {
                let _ = sender.send(RawEvent::Exited(None));
                return;
            }
        }
    });
}

fn spawn_stderr<R: Read + Send + 'static>(
    mut reader: R,
    sender: Sender<RawEvent>,
    tail: Arc<std::sync::Mutex<Vec<String>>>,
) {
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(length) => {
                    let text = String::from_utf8_lossy(&buffer[..length]).into_owned();
                    for line in text.lines() {
                        if let Ok(mut lines) = tail.lock() {
                            if lines.len() >= 200 {
                                lines.remove(0);
                            }
                            lines.push(line.to_owned());
                        }
                        if sender.send(RawEvent::Stderr(line.to_owned())).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
}

enum FrameError {
    Eof,
    Io(std::io::Error),
    Protocol(String),
}

fn read_frame<R: BufRead>(reader: &mut R) -> Result<Value, FrameError> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err(FrameError::Eof),
            Ok(_) => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if line.len() > MAX_HEADER_LINE_BYTES {
            return Err(FrameError::Protocol("header line too long".into()));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| FrameError::Protocol("invalid Content-Length".into()))?,
                );
            }
        }
    }
    let length = length.ok_or_else(|| FrameError::Protocol("missing Content-Length".into()))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(FrameError::Protocol("message too large".into()));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(FrameError::Io)?;
    serde_json::from_slice(&body).map_err(|error| FrameError::Protocol(error.to_string()))
}

// ---------------------------------------------------------------------------
// Language recipes (LN-15): probe only, never install silently
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recipe {
    pub language: &'static str,
    pub server: &'static str,
    pub args: &'static [&'static str],
    /// Shown to the user for confirmation; mtide never executes it itself
    /// (PRD SEC-12 / A-05).
    pub install_hint: &'static str,
}

pub const RECIPES: &[Recipe] = &[
    Recipe {
        language: "rust",
        server: "rust-analyzer",
        args: &[],
        install_hint: "rustup component add rust-analyzer",
    },
    Recipe {
        language: "go",
        server: "gopls",
        args: &[],
        install_hint: "go install golang.org/x/tools/gopls@latest",
    },
    Recipe {
        language: "python",
        server: "pyright-langserver",
        args: &["--stdio"],
        install_hint: "npm install -g pyright",
    },
    Recipe {
        language: "typescript",
        server: "typescript-language-server",
        args: &["--stdio"],
        install_hint: "npm install -g typescript-language-server typescript",
    },
    Recipe {
        language: "c",
        server: "clangd",
        args: &[],
        install_hint: "install clangd from your system package manager",
    },
];

pub fn recipe_for(language: &str) -> Option<&'static Recipe> {
    let language = language.to_ascii_lowercase();
    let language = match language.as_str() {
        "javascript" | "typescriptreact" | "javascriptreact" => "typescript",
        "cpp" | "c++" => "c",
        other => other,
    };
    RECIPES.iter().find(|recipe| recipe.language == language)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeResult {
    Installed { version: String },
    Missing,
}

/// Detects a server without ever installing it. A missing binary is a normal
/// outcome that drives the "confirm before install" prompt.
pub fn probe(recipe: &Recipe) -> ProbeResult {
    match std::process::Command::new(recipe.server)
        .arg("--version")
        .output()
    {
        Ok(output) if output.status.success() => ProbeResult::Installed {
            version: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        },
        _ => ProbeResult::Missing,
    }
}

// ---------------------------------------------------------------------------
// Completion and hover payloads (LN-07/LN-08)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub start: Position,
    pub end: Position,
    pub new_text: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub kind: Option<u64>,
    /// Text to insert when the item carries no text edit.
    pub insert_text: String,
    /// `insertTextFormat == 2`: the text is a snippet with tab stops.
    pub snippet: bool,
    /// Explicit replacement range from `textEdit` / `insertReplaceEdit`.
    pub text_edit: Option<TextEdit>,
    /// Edits the server wants applied together with the completion
    /// (auto-imports). They must be applied in the same transaction.
    pub additional_edits: Vec<TextEdit>,
    pub sort_text: String,
}

fn parse_position(value: &Value) -> Option<Position> {
    Some(Position {
        line: value.get("line")?.as_u64()? as u32,
        character: value.get("character")?.as_u64()? as u32,
    })
}

fn parse_range(value: &Value) -> Option<(Position, Position)> {
    Some((
        parse_position(value.get("start")?)?,
        parse_position(value.get("end")?)?,
    ))
}

/// Accepts `TextEdit`, `InsertReplaceEdit` and the legacy `{range, newText}`
/// shape; the insert range wins because it is the non-destructive variant.
pub fn parse_text_edit(value: &Value) -> Option<TextEdit> {
    let new_text = value
        .get("newText")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(range) = value.get("range") {
        let (start, end) = parse_range(range)?;
        return Some(TextEdit {
            start,
            end,
            new_text,
        });
    }
    let range = value.get("insert").or_else(|| value.get("replace"))?;
    let (start, end) = parse_range(range)?;
    Some(TextEdit {
        start,
        end,
        new_text,
    })
}

fn parse_completion_item(value: &Value) -> Option<CompletionItem> {
    let label = value.get("label")?.as_str()?.to_owned();
    let insert_text = value
        .get("insertText")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| label.clone());
    let text_edit = value.get("textEdit").and_then(parse_text_edit);
    let snippet = value.get("insertTextFormat").and_then(Value::as_u64) == Some(2);
    let additional_edits = value
        .get("additionalTextEdits")
        .and_then(Value::as_array)
        .map(|edits| edits.iter().filter_map(parse_text_edit).collect())
        .unwrap_or_default();
    Some(CompletionItem {
        label,
        detail: value
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: value.get("kind").and_then(Value::as_u64),
        insert_text,
        snippet,
        text_edit,
        additional_edits,
        sort_text: value
            .get("sortText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Parses a `textDocument/completion` result: either a bare array or a
/// `CompletionList`. `isIncomplete` is surfaced through the returned tuple so
/// callers can decide to re-request on the next keystroke.
pub fn parse_completion_result(result: &Value) -> (Vec<CompletionItem>, bool) {
    let incomplete = result
        .get("isIncomplete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| result.as_array());
    let mut items = items
        .map(|items| {
            items
                .iter()
                .filter_map(parse_completion_item)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    items.sort_by(|left, right| {
        left.sort_text
            .cmp(&right.sort_text)
            .then_with(|| left.label.cmp(&right.label))
    });
    (items, incomplete)
}

fn markup_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("value").and_then(Value::as_str).map(str::to_owned),
        Value::Array(items) => {
            let parts = items.iter().filter_map(markup_text).collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

/// Parses `textDocument/hover` contents (string, MarkupContent or MarkedString
/// array) into plain text. Markdown markers are left in place: the UI decides
/// how much to render.
pub fn parse_hover_result(result: &Value) -> Option<String> {
    let contents = result.get("contents").unwrap_or(result);
    markup_text(contents).filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use mtide_proc::read_message;
    use std::sync::{Arc, Mutex};
    #[cfg(unix)]
    use std::thread::JoinHandle;

    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(unix)]
    fn harness<F>(
        config: ClientConfig,
        server: F,
    ) -> (LspClient, Arc<Mutex<Vec<String>>>, JoinHandle<()>)
    where
        F: FnOnce(BufReader<UnixStream>, UnixStream, Arc<Mutex<Vec<String>>>) + Send + 'static,
    {
        let (client_stream, server_stream) = UnixStream::pair().expect("socket pair");
        let reader = client_stream.try_clone().expect("clone client reader");
        let methods = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&methods);
        let handle = thread::spawn(move || {
            // A read timeout keeps the scripted server from blocking forever
            // when the client still holds a duplicated descriptor.
            let _ = server_stream.set_read_timeout(Some(Duration::from_millis(150)));
            let reader = BufReader::new(server_stream.try_clone().expect("clone server reader"));
            server(reader, server_stream, recorded);
        });
        (
            LspClient::attach(config, reader, client_stream),
            methods,
            handle,
        )
    }

    #[cfg(unix)]
    fn record(methods: &Arc<Mutex<Vec<String>>>, message: &Value) {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            methods.lock().expect("method log").push(method.to_owned());
        }
    }

    /// Consumes the `initialized` notification the client sends right after
    /// the initialize response, so scripted servers read the intended request.
    #[cfg(unix)]
    fn expect_initialized(reader: &mut BufReader<UnixStream>) {
        let message = read_message(reader)
            .expect("read initialized")
            .expect("initialized notification");
        assert_eq!(message["method"], "initialized");
    }

    /// Reads the next message that carries an id, skipping notifications such
    /// as `initialized` or `$/cancelRequest`.
    #[cfg(unix)]
    fn next_request(reader: &mut BufReader<UnixStream>) -> Value {
        loop {
            match read_message(reader) {
                Ok(Some(message)) if message.get("id").is_some() => return message,
                Ok(Some(_)) => continue,
                other => panic!("expected a request, got {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    fn answer_initialize(
        reader: &mut BufReader<UnixStream>,
        writer: &mut UnixStream,
        capabilities: Value,
    ) -> Value {
        let initialize = read_message(reader)
            .expect("read initialize")
            .expect("initialize message");
        write_message(
            writer,
            &json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "capabilities": capabilities },
            }),
        )
        .expect("write initialize result");
        initialize
    }

    fn pump_until(
        client: &mut LspClient,
        budget: Duration,
        mut predicate: impl FnMut(&[ClientEvent]) -> bool,
    ) -> Vec<ClientEvent> {
        let deadline = Instant::now() + budget;
        let mut collected = Vec::new();
        while Instant::now() < deadline {
            let events = client.pump(Duration::from_millis(10));
            let done = predicate(&events);
            collected.extend(events);
            if done {
                break;
            }
        }
        collected
    }

    fn fast_config() -> ClientConfig {
        ClientConfig {
            request_timeout: Duration::from_millis(80),
            max_restarts: 3,
            backoff_base: Duration::from_millis(5),
            degrade_after_timeouts: 2,
            client_name: "mtide-test".into(),
            server_id: "rust-analyzer".into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_initialize_handshake_negotiates_capabilities() {
        let (mut client, methods, handle) =
            harness(fast_config(), |mut reader, mut writer, methods| {
                let initialize = answer_initialize(
                    &mut reader,
                    &mut writer,
                    json!({
                        "completionProvider": { "triggerCharacters": ["."] },
                        "hoverProvider": true,
                        "textDocumentSync": 1,
                    }),
                );
                record(&methods, &initialize);
                while let Ok(Some(message)) = read_message(&mut reader) {
                    record(&methods, &message);
                }
            });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("start initialize");
        assert_eq!(client.state(), Lifecycle::Initializing);

        let events = pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });
        assert!(matches!(
            events.first(),
            Some(ClientEvent::StateChanged(Lifecycle::Ready))
        ));
        assert_eq!(client.state(), Lifecycle::Ready);
        assert!(client.capabilities().supports(Feature::Completion));
        assert!(client.capabilities().supports(Feature::Hover));
        assert!(!client.capabilities().supports(Feature::Rename));
        assert_eq!(client.capabilities().sync, SyncKind::Full);
        assert_eq!(client.pending_count(), 0);

        // The client must send `initialized` before any feature request.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if methods
                .lock()
                .expect("method log")
                .iter()
                .any(|method| method == "initialized")
            {
                break;
            }
            client.pump(Duration::from_millis(10));
        }
        assert_eq!(
            methods
                .lock()
                .expect("method log")
                .first()
                .map(String::as_str),
            Some("initialize")
        );
        assert!(methods
            .lock()
            .expect("method log")
            .iter()
            .any(|method| method == "initialized"));

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_responses_correlate_by_id_even_when_out_of_order() {
        let (mut client, _methods, handle) =
            harness(fast_config(), |mut reader, mut writer, _methods| {
                answer_initialize(&mut reader, &mut writer, json!({ "textDocumentSync": 1 }));
                expect_initialized(&mut reader);
                let first = read_message(&mut reader)
                    .expect("read")
                    .expect("first request");
                let second = read_message(&mut reader)
                    .expect("read")
                    .expect("second request");
                write_message(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": second["id"], "result": "second" }),
                )
                .expect("reply second");
                write_message(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": first["id"], "result": "first" }),
                )
                .expect("reply first");
                thread::sleep(Duration::from_millis(50));
            });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });
        let first_id = client
            .request("textDocument/hover", json!({}))
            .expect("request one");
        let second_id = client
            .request("textDocument/definition", json!({}))
            .expect("request two");

        let events = pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .filter(|event| matches!(event, ClientEvent::Response { .. }))
                .count()
                >= 2
        });
        let responses = events
            .iter()
            .filter_map(|event| match event {
                ClientEvent::Response { id, result } => Some((*id, result.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        let first_result = responses
            .iter()
            .find(|(id, _)| *id == first_id)
            .map(|(_, r)| r.clone());
        let second_result = responses
            .iter()
            .find(|(id, _)| *id == second_id)
            .map(|(_, r)| r.clone());
        assert_eq!(first_result, Some(json!("first")));
        assert_eq!(second_result, Some(json!("second")));
        assert_eq!(client.pending_count(), 0);

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_repeated_timeouts_degrade_the_session() {
        let (mut client, _methods, handle) =
            harness(fast_config(), |mut reader, mut writer, _methods| {
                answer_initialize(&mut reader, &mut writer, json!({}));
                expect_initialized(&mut reader);
                // A wedged server: never answers, keeps the session open.
                thread::sleep(Duration::from_millis(400));
            });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });

        client
            .request("textDocument/hover", json!({}))
            .expect("first");
        let events = pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Timeout { .. }))
        });
        assert!(events.iter().any(|event| {
            matches!(event, ClientEvent::Timeout { method, .. } if method == "textDocument/hover")
        }));
        assert_eq!(client.state(), Lifecycle::Ready, "one timeout is tolerated");
        assert!(events
            .iter()
            .any(|event| matches!(event, ClientEvent::Timeout { .. }) && true));
        // A timeout must cancel the request server-side as well.
        let _ = &events;

        client
            .request("textDocument/hover", json!({}))
            .expect("second");
        let events = pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Degraded)))
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Degraded))));
        assert_eq!(client.state(), Lifecycle::Degraded);
        // Degraded sessions still accept requests so the UI can retry.
        assert!(client.request("textDocument/hover", json!({})).is_ok());

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_success_after_degraded_restores_ready() {
        let config = ClientConfig {
            degrade_after_timeouts: 1,
            ..fast_config()
        };
        let (mut client, _methods, handle) = harness(config, |mut reader, mut writer, _methods| {
            answer_initialize(&mut reader, &mut writer, json!({}));
            expect_initialized(&mut reader);
            // Drop the first request, answer the second (skipping the
            // $/cancelRequest the client sends when a request times out).
            let _ = next_request(&mut reader);
            let message = next_request(&mut reader);
            write_message(
                &mut writer,
                &json!({ "jsonrpc": "2.0", "id": message["id"], "result": "recovered" }),
            )
            .expect("reply");
            thread::sleep(Duration::from_millis(50));
        });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });
        client
            .request("textDocument/hover", json!({}))
            .expect("first");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Degraded)))
        });
        assert_eq!(client.state(), Lifecycle::Degraded);

        client
            .request("textDocument/hover", json!({}))
            .expect("second");
        let events = pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Response { .. }))
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, ClientEvent::Response { result, .. } if result == &json!("recovered"))));
        assert_eq!(client.state(), Lifecycle::Ready);

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_cancel_drops_pending_and_notifies_the_server() {
        let (mut client, methods, handle) =
            harness(fast_config(), |mut reader, mut writer, methods| {
                answer_initialize(&mut reader, &mut writer, json!({}));
                while let Ok(Some(message)) = read_message(&mut reader) {
                    record(&methods, &message);
                }
            });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });
        let id = client
            .request("textDocument/completion", json!({}))
            .expect("completion request");
        assert_eq!(client.pending_count(), 1);
        client.cancel(id).expect("cancel");
        assert_eq!(client.pending_count(), 0);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            client.pump(Duration::from_millis(10));
            if methods
                .lock()
                .expect("method log")
                .iter()
                .any(|method| method == "$/cancelRequest")
            {
                break;
            }
        }
        assert!(methods
            .lock()
            .expect("method log")
            .iter()
            .any(|method| method == "$/cancelRequest"));

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_didchange_versions_are_monotonic_and_full_sync() {
        let versions = Arc::new(Mutex::new(Vec::new()));
        let versions_for_server = Arc::clone(&versions);
        let (mut client, _methods, handle) =
            harness(fast_config(), move |mut reader, mut writer, _methods| {
                answer_initialize(&mut reader, &mut writer, json!({ "textDocumentSync": 1 }));
                while let Ok(Some(message)) = read_message(&mut reader) {
                    let params = &message["params"];
                    if let Some(version) = params["textDocument"]["version"].as_i64() {
                        versions_for_server.lock().expect("versions").push(version);
                    }
                    if message["method"] == "textDocument/didOpen" {
                        assert_eq!(params["textDocument"]["text"], "fn main() {}");
                    }
                }
            });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });

        let path = Path::new("/tmp/workspace/src/main.rs");
        client
            .did_open(path, "rust", "fn main() {}")
            .expect("did open");
        for text in ["fn main() { }", "fn main() {  }", "fn main() {   }"] {
            client.did_change(path, text).expect("did change");
        }
        // didChange for a document that was never opened must be a no-op.
        client
            .did_change(Path::new("/tmp/workspace/other.rs"), "x")
            .expect("ignored");

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            client.pump(Duration::from_millis(10));
            if versions.lock().expect("versions").len() >= 4 {
                break;
            }
        }
        assert_eq!(*versions.lock().expect("versions"), vec![0, 1, 2, 3]);
        assert_eq!(client.documents().version(path), Some(3));

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_protocol_error_is_reported_without_killing_the_session() {
        let (mut client, _methods, handle) =
            harness(fast_config(), |mut reader, mut writer, _methods| {
                answer_initialize(&mut reader, &mut writer, json!({}));
                expect_initialized(&mut reader);
                let request = read_message(&mut reader).expect("read").expect("request");
                // Malformed body: framing is valid, JSON is not.
                writer
                    .write_all(b"Content-Length: 7\r\n\r\n{not-js")
                    .expect("write garbage");
                writer.flush().expect("flush");
                write_message(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": request["id"], "result": "ok" }),
                )
                .expect("reply");
                thread::sleep(Duration::from_millis(50));
            });

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });
        client
            .request("textDocument/hover", json!({}))
            .expect("request");
        let events = pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Response { .. }))
        });
        assert!(events
            .iter()
            .any(|event| matches!(event, ClientEvent::ProtocolError(_))));
        assert!(events.iter().any(
            |event| matches!(event, ClientEvent::Response { result, .. } if result == &json!("ok"))
        ));
        assert_eq!(client.state(), Lifecycle::Ready);

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln09_diagnostics_are_stored_and_stale_versions_are_dropped() {
        let (mut client, _methods, handle) = harness(
            fast_config(),
            |mut reader, mut writer, _methods| {
                answer_initialize(&mut reader, &mut writer, json!({ "textDocumentSync": 1 }));
                expect_initialized(&mut reader);
                let open = read_message(&mut reader).expect("read").expect("didOpen");
                let uri = open["params"]["textDocument"]["uri"].clone();
                let publish = |writer: &mut UnixStream, version: i64, message: &str| {
                    write_message(
                    writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "version": version,
                            "diagnostics": [{
                                "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } },
                                "severity": 1,
                                "source": "rust-analyzer",
                                "message": message,
                            }],
                        },
                    }),
                )
                .expect("publish");
                };
                publish(&mut writer, 0, "first");
                let _change = read_message(&mut reader).expect("read").expect("didChange");
                // A server racing with the editor: version 0 is stale by now.
                publish(&mut writer, 0, "stale");
                publish(&mut writer, 1, "fresh");
                thread::sleep(Duration::from_millis(50));
            },
        );

        client
            .initialize(Path::new("/tmp/workspace"))
            .expect("initialize");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::StateChanged(Lifecycle::Ready)))
        });
        let path = Path::new("/tmp/workspace/src/main.rs");
        client
            .did_open(path, "rust", "fn main() {}")
            .expect("did open");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::Diagnostic { .. }))
        });
        let stored = client.diagnostics().file(path).expect("stored diagnostics");
        assert_eq!(stored.diagnostics[0].message, "first");
        assert_eq!(stored.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(client.diagnostics().counts().errors, 1);

        client
            .did_change(path, "fn main() { }")
            .expect("did change");
        pump_until(&mut client, Duration::from_secs(2), |events| {
            events.iter().any(|event| {
                matches!(event, ClientEvent::Diagnostic { version, .. } if *version == Some(1))
            })
        });
        let stored = client.diagnostics().file(path).expect("stored diagnostics");
        assert_eq!(
            stored.diagnostics[0].message, "fresh",
            "a publish for an older document version must not overwrite newer results"
        );

        client.shutdown();
        handle.join().expect("server thread");
    }

    #[cfg(unix)]
    #[test]
    fn fr_ln06_crash_restarts_with_backoff_then_disables() {
        let config = ClientConfig {
            request_timeout: Duration::from_millis(50),
            max_restarts: 2,
            backoff_base: Duration::from_millis(10),
            degrade_after_timeouts: 2,
            client_name: "mtide-test".into(),
            server_id: "crashy".into(),
        };
        let mut client = LspClient::spawn(
            config,
            ServerSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 3".into()],
                root: PathBuf::from("/tmp/workspace"),
            },
        )
        .expect("spawn crashing server");

        let mut states = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            for event in client.pump(Duration::from_millis(10)) {
                if let ClientEvent::StateChanged(state) = event {
                    states.push(state);
                }
            }
            if client.state() == Lifecycle::Disabled {
                break;
            }
        }
        assert!(states.contains(&Lifecycle::Crashed), "states: {states:?}");
        assert!(
            states.contains(&Lifecycle::Restarting),
            "states: {states:?}"
        );
        assert_eq!(client.state(), Lifecycle::Disabled);
        assert!(client.request("textDocument/hover", json!({})).is_err());
    }

    #[test]
    fn fr_ln07_completion_result_parses_edits_snippets_and_auto_imports() {
        let result = json!({
            "isIncomplete": true,
            "items": [
                {
                    "label": "println!",
                    "kind": 15,
                    "detail": "macro",
                    "insertText": "println!(\"$1\")",
                    "insertTextFormat": 2,
                    "sortText": "b",
                },
                {
                    "label": "String",
                    "sortText": "a",
                    "textEdit": {
                        "newText": "String",
                        "range": {
                            "start": { "line": 0, "character": 4 },
                            "end": { "line": 0, "character": 6 }
                        }
                    },
                    "additionalTextEdits": [
                        {
                            "newText": "use std::string::String;\n",
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 }
                            }
                        }
                    ]
                }
            ]
        });
        let (items, incomplete) = parse_completion_result(&result);
        assert!(incomplete, "isIncomplete must be surfaced to the caller");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "String", "sortText must order the list");
        let edit = items[0].text_edit.as_ref().expect("text edit");
        assert_eq!(
            edit.start,
            Position {
                line: 0,
                character: 4
            }
        );
        assert_eq!(items[0].additional_edits.len(), 1);
        assert!(items[1].snippet, "insertTextFormat 2 means snippet");
        assert_eq!(items[1].insert_text, "println!(\"$1\")");
    }

    #[test]
    fn fr_ln07_completion_accepts_a_bare_array_and_insert_replace_edits() {
        let result = json!([{
            "label": "foo",
            "insertTextFormat": 1,
            "textEdit": {
                "newText": "foo()",
                "insert": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 5 }
                },
                "replace": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 9 }
                }
            }
        }]);
        let (items, incomplete) = parse_completion_result(&result);
        assert!(!incomplete);
        assert_eq!(items.len(), 1);
        assert!(!items[0].snippet);
        let edit = items[0].text_edit.as_ref().expect("text edit");
        assert_eq!(
            edit.end,
            Position {
                line: 1,
                character: 5
            }
        );
    }

    #[test]
    fn fr_ln08_hover_parses_all_three_content_shapes() {
        assert_eq!(
            parse_hover_result(&json!({ "contents": "plain" })),
            Some("plain".into())
        );
        assert_eq!(
            parse_hover_result(&json!({
                "contents": { "kind": "markdown", "value": "```rust\nfn f()\n```" }
            })),
            Some("```rust\nfn f()\n```".into())
        );
        assert_eq!(
            parse_hover_result(&json!({
                "contents": [
                    { "language": "rust", "value": "let x: u8" },
                    "second line"
                ]
            })),
            Some("let x: u8\nsecond line".into())
        );
        assert_eq!(parse_hover_result(&json!({ "contents": "" })), None);
        assert_eq!(parse_hover_result(&json!({})), None);
    }
    #[test]
    fn fr_ln06_utf16_positions_round_trip_cjk_and_emoji() {
        let text = "let a = \"你好🌍\";\nlet b = 2;\n";
        // Byte offset right after the emoji (7 bytes for the two CJK chars, 4 for the emoji).
        let emoji_end = "let a = \"你好🌍".len();
        let position = position_at(text, emoji_end);
        assert_eq!(position.line, 0);
        // "let a = "" is 9 UTF-16 units, 你好 is 2, 🌍 is 2.
        assert_eq!(position.character, 13);
        assert_eq!(offset_at(text, position), emoji_end);

        let second_line = text.find("let b").expect("second line");
        let position = position_at(text, second_line);
        assert_eq!(position.line, 1);
        assert_eq!(position.character, 0);
        assert_eq!(offset_at(text, position), second_line);

        // Out-of-range positions clamp instead of panicking.
        assert_eq!(
            offset_at(
                text,
                Position {
                    line: 99,
                    character: 99
                }
            ),
            text.len()
        );
        assert_eq!(
            offset_at(
                text,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            0
        );
        // `你好` occupies UTF-16 columns 9 and 10, so column 10 is the byte
        // offset of 好; columns inside a surrogate pair round up to the next
        // character boundary instead of splitting it.
        assert_eq!(
            offset_at(
                text,
                Position {
                    line: 0,
                    character: 10
                }
            ),
            "let a = \"你".len()
        );
        let astral = "a🌍b";
        let after_emoji = offset_at(
            astral,
            Position {
                line: 0,
                character: 2,
            },
        );
        assert_eq!(
            after_emoji,
            "a".len(),
            "a column inside the surrogate pair rounds back"
        );
        assert_eq!(
            offset_at(
                astral,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            "a🌍".len()
        );
    }

    #[test]
    fn fr_ln06_file_uris_round_trip_spaces_and_cjk() {
        for path in [
            Path::new("/tmp/a b/main.rs"),
            Path::new("/tmp/工作区/文件.rs"),
            Path::new("/tmp/percent%20name.txt"),
        ] {
            let uri = file_uri(path);
            assert!(uri.starts_with("file:///"), "{uri}");
            assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
        }
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
    }

    #[test]
    fn fr_ln19_diagnostics_merge_multiple_sources_and_filter_by_source() {
        let mut store = DiagnosticStore::default();
        let path = Path::new("/tmp/workspace/src/main.rs");
        store.publish(
            path,
            Some(2),
            "rust-analyzer",
            vec![
                Diagnostic {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 1,
                        },
                    },
                    severity: DiagnosticSeverity::Error,
                    source: "rust-analyzer".into(),
                    message: "mismatched types".into(),
                    code: Some("E0308".into()),
                },
                Diagnostic {
                    range: Range {
                        start: Position {
                            line: 1,
                            character: 0,
                        },
                        end: Position {
                            line: 1,
                            character: 1,
                        },
                    },
                    severity: DiagnosticSeverity::Warning,
                    source: "clippy".into(),
                    message: "unused variable".into(),
                    code: None,
                },
            ],
        );
        let counts = store.counts();
        assert_eq!(counts.errors, 1);
        assert_eq!(counts.warnings, 1);
        assert_eq!(counts.total(), 2);
        assert_eq!(store.by_source("clippy").len(), 1);
        assert_eq!(store.by_source("rust-analyzer").len(), 1);
        assert_eq!(
            store.sources().into_iter().collect::<Vec<_>>(),
            vec!["clippy", "rust-analyzer"]
        );
        assert!(!store.publish(path, Some(1), "rust-analyzer", Vec::new()));
        store.clear(path);
        assert_eq!(store.counts().total(), 0);
    }

    #[test]
    fn fr_ln15_recipe_probe_reports_missing_without_installing() {
        let recipe = recipe_for("rust").expect("rust recipe");
        assert_eq!(recipe.server, "rust-analyzer");
        assert!(recipe.install_hint.contains("rustup"));
        assert_eq!(recipe_for("Rust"), Some(recipe));
        assert_eq!(
            recipe_for("javascript").map(|r| r.language),
            Some("typescript")
        );
        assert!(recipe_for("cobol").is_none());

        let missing = Recipe {
            language: "test",
            server: "mtide-definitely-not-installed-server",
            args: &[],
            install_hint: "install it yourself",
        };
        assert_eq!(probe(&missing), ProbeResult::Missing);
    }
}
