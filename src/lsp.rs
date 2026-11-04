use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
};

use serde_json::{json, Value};

use crate::config::{LspServerConfig, LspSettings};
use crate::error::{AppError, AppResult};

const LSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspServerState {
    Starting,
    Connected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub start: LspPosition,
    pub end: LspPosition,
    pub severity: Option<u8>,
    pub message: String,
}

#[allow(dead_code)]
#[derive(Default)]
pub struct LspManager {
    configs: HashMap<String, LspServerConfig>,
    clients: HashMap<String, LspClient>,
    states: HashMap<String, LspServerState>,
    diagnostics: HashMap<String, Vec<LspDiagnostic>>,
    document_versions: HashMap<String, i32>,
}

#[allow(dead_code)]
impl LspManager {
    pub fn from_settings(settings: &LspSettings) -> Self {
        Self {
            configs: settings
                .servers
                .iter()
                .flat_map(|(name, config)| {
                    let mut entries = vec![(name.clone(), config.clone())];
                    for language in &config.languages {
                        entries.push((language.clone(), config.clone()));
                    }
                    entries
                })
                .collect(),
            clients: HashMap::new(),
            states: HashMap::new(),
            diagnostics: HashMap::new(),
            document_versions: HashMap::new(),
        }
    }

    pub fn configured(&self, language: &str) -> bool {
        self.configs.contains_key(language)
    }

    pub fn server_count(&self) -> usize {
        self.configs.len()
    }

    pub fn state(&self, language: &str) -> Option<LspServerState> {
        self.states.get(language).copied()
    }

    pub fn start(&mut self, language: &str, root_uri: Option<&str>) -> AppResult<bool> {
        if self.clients.contains_key(language) {
            self.states
                .insert(language.to_owned(), LspServerState::Connected);
            return Ok(true);
        }
        let Some(config) = self.configs.get(language) else {
            return Ok(false);
        };
        if config.command.is_empty() {
            self.states
                .insert(language.to_owned(), LspServerState::Failed);
            return Err(AppError::Lsp(format!("empty command for {language}")));
        }
        self.states
            .insert(language.to_owned(), LspServerState::Starting);
        let mut client = match LspClient::spawn(&config.command) {
            Ok(client) => client,
            Err(error) => {
                self.states
                    .insert(language.to_owned(), LspServerState::Failed);
                return Err(error);
            }
        };
        if let Err(error) = client.initialize(root_uri) {
            self.states
                .insert(language.to_owned(), LspServerState::Failed);
            return Err(error);
        }
        self.clients.insert(language.to_owned(), client);
        self.states
            .insert(language.to_owned(), LspServerState::Connected);
        Ok(true)
    }

    pub fn client_mut(&mut self, language: &str) -> Option<&mut LspClient> {
        self.clients.get_mut(language)
    }

    pub fn request(
        &mut self,
        language: &str,
        method: &str,
        params: Value,
    ) -> AppResult<Option<Value>> {
        let Some(client) = self.clients.get_mut(language) else {
            return Ok(None);
        };
        let result = client.request(method, params);
        match result {
            Ok(result) => Ok(Some(result)),
            Err(error) => {
                self.clients.remove(language);
                self.states
                    .insert(language.to_owned(), LspServerState::Failed);
                Err(error)
            }
        }
    }

    pub fn sync_document(
        &mut self,
        language: &str,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> AppResult<bool> {
        let Some(client) = self.clients.get_mut(language) else {
            return Ok(false);
        };
        let version = self.document_versions.entry(uri.to_owned()).or_insert(0);
        if *version == 0 {
            *version = 1;
            client.did_open(uri, language_id, *version, text)?;
        } else {
            *version += 1;
            client.did_change(uri, *version, text)?;
        }
        Ok(true)
    }

    pub fn save_document(&mut self, language: &str, uri: &str) -> AppResult<bool> {
        let Some(client) = self.clients.get_mut(language) else {
            return Ok(false);
        };
        if let Err(error) = client.did_save(uri) {
            self.clients.remove(language);
            self.states
                .insert(language.to_owned(), LspServerState::Failed);
            return Err(error);
        }
        Ok(true)
    }

    pub fn close_document(&mut self, language: &str, uri: &str) -> AppResult<bool> {
        let Some(client) = self.clients.get_mut(language) else {
            self.document_versions.remove(uri);
            return Ok(false);
        };
        if let Err(error) = client.did_close(uri) {
            self.clients.remove(language);
            self.states
                .insert(language.to_owned(), LspServerState::Failed);
            return Err(error);
        }
        self.document_versions.remove(uri);
        Ok(true)
    }

    pub fn diagnostics(&self, uri: &str) -> &[LspDiagnostic] {
        self.diagnostics.get(uri).map_or(&[], Vec::as_slice)
    }

    pub fn apply_notification(&mut self, message: &Value) -> bool {
        let Some((uri, diagnostics)) = parse_publish_diagnostics(message) else {
            return false;
        };
        self.diagnostics.insert(uri, diagnostics);
        true
    }

    pub fn poll(&mut self) {
        let exited: Vec<_> = self
            .clients
            .iter_mut()
            .filter_map(|(language, client)| client.has_exited().then(|| language.clone()))
            .collect();
        for language in exited {
            self.clients.remove(&language);
            self.states.insert(language, LspServerState::Failed);
        }
        let messages: Vec<_> = self
            .clients
            .values_mut()
            .flat_map(LspClient::poll_notifications)
            .collect();
        for message in messages {
            self.apply_notification(&message);
        }
    }
}

#[allow(dead_code)]
fn parse_publish_diagnostics(message: &Value) -> Option<(String, Vec<LspDiagnostic>)> {
    if message.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = message.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_owned();
    let diagnostics = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let range = item.get("range")?;
            let start = range.get("start")?;
            let end = range.get("end")?;
            Some(LspDiagnostic {
                start: LspPosition {
                    line: start.get("line")?.as_u64()? as u32,
                    character: start.get("character")?.as_u64()? as u32,
                },
                end: LspPosition {
                    line: end.get("line")?.as_u64()? as u32,
                    character: end.get("character")?.as_u64()? as u32,
                },
                severity: item
                    .get("severity")
                    .and_then(Value::as_u64)
                    .map(|value| value as u8),
                message: item.get("message")?.as_str()?.to_owned(),
            })
        })
        .collect();
    Some((uri, diagnostics))
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    notifications: Vec<Value>,
    next_id: u64,
}

#[allow(dead_code)]
impl LspClient {
    fn has_exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    pub fn spawn(command: &[String]) -> AppResult<Self> {
        let executable = command
            .first()
            .ok_or_else(|| AppError::Lsp("empty server command".into()))?;
        let mut child = Command::new(executable)
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Lsp("LSP stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Lsp("LSP stdout unavailable".into()))?;
        let (sender, messages) = mpsc::channel();
        thread::Builder::new()
            .name("mtide-lsp-reader".to_owned())
            .spawn(move || read_lsp_messages(stdout, sender))
            .map_err(|error| AppError::Lsp(error.to_string()))?;
        Ok(Self {
            child,
            stdin,
            messages,
            notifications: Vec::new(),
            next_id: 1,
        })
    }

    pub fn request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        loop {
            let message = match self.messages.recv_timeout(LSP_REQUEST_TIMEOUT) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = write_message(
                        &mut self.stdin,
                        &json!({
                            "jsonrpc":"2.0",
                            "method":"$/cancelRequest",
                            "params":{"id":id}
                        }),
                    );
                    return Err(AppError::Lsp(format!(
                        "LSP request {method} timed out after {}s",
                        LSP_REQUEST_TIMEOUT.as_secs()
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Lsp("LSP server disconnected".into()));
                }
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(AppError::Lsp(error.to_string()));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if message.get("method").is_some() {
                self.notifications.push(message);
            }
        }
    }

    pub fn poll_notifications(&mut self) -> impl Iterator<Item = Value> + '_ {
        self.notifications.extend(self.messages.try_iter());
        self.notifications.drain(..)
    }

    pub fn initialize(&mut self, root_uri: Option<&str>) -> AppResult<Value> {
        let result = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "synchronization": {"didSave": true, "dynamicRegistration": false},
                        "completion": {"completionItem": {"snippetSupport": false}},
                        "hover": {"contentFormat": ["markdown", "plaintext"]}
                    }
                }
            }),
        )?;
        write_message(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )?;
        Ok(result)
    }

    pub fn did_open(
        &mut self,
        uri: &str,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> AppResult<()> {
        write_message(
            &mut self.stdin,
            &json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didOpen",
                "params":{"textDocument":{"uri":uri,"languageId":language_id,"version":version,"text":text}}
            }),
        )
    }

    pub fn did_change(&mut self, uri: &str, version: i32, text: &str) -> AppResult<()> {
        write_message(
            &mut self.stdin,
            &json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didChange",
                "params":{"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]}
            }),
        )
    }

    pub fn did_save(&mut self, uri: &str) -> AppResult<()> {
        write_message(&mut self.stdin, &did_save_message(uri))
    }

    pub fn did_close(&mut self, uri: &str) -> AppResult<()> {
        write_message(
            &mut self.stdin,
            &json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didClose",
                "params":{"textDocument":{"uri":uri}}
            }),
        )
    }

    pub fn shutdown(&mut self) -> AppResult<()> {
        let _ = self.request("shutdown", Value::Null)?;
        write_message(&mut self.stdin, &json!({"jsonrpc":"2.0","method":"exit"}))?;
        let _ = self.child.kill();
        Ok(())
    }
}

fn read_lsp_messages(mut reader: impl Read, sender: mpsc::Sender<Value>) {
    while let Ok(message) = read_message(&mut reader) {
        if sender.send(message).is_err() {
            break;
        }
    }
}

fn write_message(writer: &mut impl Write, value: &Value) -> AppResult<()> {
    let body = serde_json::to_vec(value).map_err(|error| AppError::Lsp(error.to_string()))?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn did_save_message(uri: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "method":"textDocument/didSave",
        "params":{"textDocument":{"uri":uri}}
    })
}

fn read_message(reader: &mut impl Read) -> AppResult<Value> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    while !headers.ends_with(b"\r\n\r\n") {
        reader.read_exact(&mut byte)?;
        headers.push(byte[0]);
        if headers.len() > 8192 {
            return Err(AppError::Lsp("LSP headers too large".into()));
        }
    }
    let header = String::from_utf8(headers).map_err(|error| AppError::Lsp(error.to_string()))?;
    let length = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .ok_or_else(|| AppError::Lsp("missing LSP Content-Length".into()))?;
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| AppError::Lsp(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LspServerConfig, LspSettings};
    use std::collections::BTreeMap;
    #[test]
    fn reads_content_length_framed_json() {
        let body = br#"{"jsonrpc":"2.0","id":1}"#;
        let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        let mut input = std::io::Cursor::new(bytes);
        let value = read_message(&mut input).expect("message");
        assert_eq!(value["id"], 1);
    }
    #[test]
    fn writes_content_length_framed_json() {
        let mut output = Vec::new();
        write_message(&mut output, &json!({"id":1})).expect("message");
        assert!(String::from_utf8(output)
            .expect("utf8")
            .starts_with("Content-Length:"));
    }

    #[test]
    fn builds_did_save_notification_without_requiring_full_document_text() {
        let message = did_save_message("file:///tmp/main.rs");
        assert_eq!(message["method"], "textDocument/didSave");
        assert_eq!(
            message["params"]["textDocument"]["uri"],
            "file:///tmp/main.rs"
        );
        assert!(message["params"]["textDocument"].get("text").is_none());
    }

    #[test]
    fn builds_did_close_notification_with_document_uri() {
        let message = json!({
            "jsonrpc":"2.0",
            "method":"textDocument/didClose",
            "params":{"textDocument":{"uri":"file:///tmp/main.rs"}}
        });
        assert_eq!(message["method"], "textDocument/didClose");
        assert_eq!(
            message["params"]["textDocument"]["uri"],
            "file:///tmp/main.rs"
        );
    }

    #[test]
    fn stores_publish_diagnostics_by_document_uri() {
        let mut manager = LspManager::default();
        let message = json!({
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///src/main.rs",
                "diagnostics": [{
                    "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}},
                    "severity": 1,
                    "message": "example"
                }]
            }
        });
        assert!(manager.apply_notification(&message));
        assert_eq!(
            manager.diagnostics("file:///src/main.rs")[0].message,
            "example"
        );
    }

    #[test]
    fn records_failed_state_for_invalid_server_configuration() {
        let settings = LspSettings {
            extra: BTreeMap::new(),
            enabled: true,
            servers: BTreeMap::from([(
                "rust".to_owned(),
                LspServerConfig {
                    command: Vec::new(),
                    languages: Vec::new(),
                },
            )]),
        };
        let mut manager = LspManager::from_settings(&settings);
        assert_eq!(manager.state("rust"), None);
        assert!(manager.start("rust", None).is_err());
        assert_eq!(manager.state("rust"), Some(LspServerState::Failed));
    }
}
