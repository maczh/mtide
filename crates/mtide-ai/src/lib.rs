//! Provider-neutral AI orchestration contracts and safety primitives.
//!
//! This crate never writes a workspace file directly. Proposed changes remain
//! data until a caller explicitly passes them through a diff gate.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub headless: bool,
    pub streaming: bool,
    pub structured: bool,
    pub tool_calls: bool,
    pub patch_output: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProbeResult {
    Installed { version: String },
    Missing,
    TooOld { found: String, minimum: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        name: String,
        ok: bool,
        summary: String,
    },
    PatchProposal {
        patch: String,
        base_hashes: BTreeMap<PathBuf, String>,
    },
    Usage {
        input: u64,
        output: u64,
    },
    Error {
        code: String,
        message: String,
    },
    Done {
        success: bool,
    },
}

pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn executable(&self) -> &'static str;
    fn capabilities(&self) -> AgentCapabilities;
    fn probe(&self) -> ProbeResult {
        probe_command(self.executable())
    }
    fn build_invocation(&self, request: &AgentRequest) -> Invocation;
    fn parse_event(&self, line: &str) -> Option<AgentEvent>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CustomAdapterSpec {
    pub id: String,
    pub display_name: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub default_args: Vec<String>,
    #[serde(default)]
    pub headless: bool,
    #[serde(default)]
    pub install_command: Option<String>,
    #[serde(default)]
    pub minimum_version: Option<String>,
}

impl CustomAdapterSpec {
    pub fn invocation(&self, request: &AgentRequest) -> Invocation {
        Invocation {
            program: self.executable.clone(),
            args: self.default_args.clone(),
            stdin: request.prompt.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinAdapter {
    Codex,
    ClaudeCode,
    CodeBuddy,
}

impl BuiltinAdapter {
    pub fn all() -> [Self; 3] {
        [Self::Codex, Self::ClaudeCode, Self::CodeBuddy]
    }
}

impl AgentAdapter for BuiltinAdapter {
    fn id(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::CodeBuddy => "codebuddy",
        }
    }
    fn display_name(&self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::ClaudeCode => "Claude Code",
            Self::CodeBuddy => "CodeBuddy",
        }
    }
    fn executable(&self) -> &'static str {
        self.id()
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            headless: true,
            streaming: true,
            structured: true,
            tool_calls: true,
            patch_output: true,
        }
    }
    fn build_invocation(&self, request: &AgentRequest) -> Invocation {
        let args = match self {
            Self::Codex => vec!["exec".into(), "--json".into()],
            Self::ClaudeCode => vec!["-p".into(), "--output-format".into(), "json".into()],
            Self::CodeBuddy => vec!["--headless".into(), "--json".into()],
        };
        Invocation {
            program: self.executable().into(),
            args,
            stdin: request.prompt.clone(),
        }
    }
    fn parse_event(&self, line: &str) -> Option<AgentEvent> {
        parse_json_event(line)
    }
}

fn probe_command(program: &str) -> ProbeResult {
    match Command::new(program).arg("--version").output() {
        Ok(output) if output.status.success() => ProbeResult::Installed {
            version: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        },
        _ => ProbeResult::Missing,
    }
}

pub fn parse_json_event(line: &str) -> Option<AgentEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let kind = value.get("type").or_else(|| value.get("event"))?.as_str()?;
    match kind {
        "text" | "message" | "delta" => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(|text| AgentEvent::TextDelta(text.into())),
        "reasoning" => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(|text| AgentEvent::ReasoningDelta(text.into())),
        "tool_call" => Some(AgentEvent::ToolCall {
            name: value.get("name")?.as_str()?.into(),
            args: value
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        "tool_result" => Some(AgentEvent::ToolResult {
            name: value.get("name")?.as_str()?.into(),
            ok: value
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            summary: value
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into(),
        }),
        "usage" => Some(AgentEvent::Usage {
            input: value
                .get("input")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            output: value
                .get("output")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        }),
        "done" | "complete" => Some(AgentEvent::Done {
            success: value
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        }),
        "error" => Some(AgentEvent::Error {
            code: value
                .get("code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("provider")
                .into(),
            message: value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into(),
        }),
        "patch" => Some(AgentEvent::PatchProposal {
            patch: value.get("patch")?.as_str()?.into(),
            base_hashes: BTreeMap::new(),
        }),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextFile {
    pub path: PathBuf,
    pub lines: usize,
    pub content: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FilteredContext {
    pub files: Vec<ContextFile>,
    pub excluded: Vec<PathBuf>,
    pub selected_text: Option<String>,
    pub diagnostics: Vec<String>,
}

pub fn collect_context(
    root: &Path,
    paths: &[PathBuf],
    selected_text: Option<String>,
    diagnostics: Vec<String>,
) -> Result<FilteredContext, AiError> {
    let ignored = load_aiignore(root);
    let mut context = FilteredContext {
        selected_text,
        diagnostics,
        ..FilteredContext::default()
    };
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        if is_sensitive(&absolute)
            || ignored
                .iter()
                .any(|pattern| matches_pattern(pattern, &absolute, root))
        {
            context.excluded.push(absolute);
            continue;
        }
        let content = fs::read_to_string(&absolute)?;
        context.files.push(ContextFile {
            path: absolute,
            lines: content.lines().count(),
            content,
        });
    }
    Ok(context)
}

pub fn is_sensitive(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == ".env" || name.starts_with(".env.") || name.ends_with(".pem") || name.ends_with(".key") || name.starts_with("id_") || name == "credentials" || name == ".netrc" || path.components().any(|part| matches!(part, std::path::Component::Normal(value) if value == ".ssh" || value == ".aws"))
}

fn load_aiignore(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join(".aiignore"))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn matches_pattern(pattern: &str, path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    pattern == relative
        || pattern
            == path
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or_default()
        || (pattern.ends_with('/') && relative.starts_with(pattern.trim_end_matches('/')))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp: u64,
    pub adapter: String,
    pub version: String,
    pub files: Vec<(PathBuf, usize)>,
    pub wrote_files: bool,
    pub accepted_hunks: usize,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditLog {
    pub records: Vec<AuditRecord>,
}

impl AuditLog {
    pub fn append(&mut self, record: AuditRecord) {
        self.records.push(record);
    }
    pub fn save_append(&self, path: &Path) -> Result<(), AiError> {
        let mut records = if path.exists() {
            Self::load(path)?.records
        } else {
            Vec::new()
        };
        records.extend(self.records.iter().cloned());
        let bytes = serde_json::to_vec_pretty(&Self { records })?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self, AiError> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
    pub fn to_markdown(&self) -> String {
        let mut output = String::from("# AI audit log\n\n| Time | Adapter | Files | Wrote | Hunk count | Elapsed ms |\n|---|---|---:|---|---:|---:|\n");
        for record in &self.records {
            output.push_str(&format!(
                "| {} | {} {} | {} | {} | {} | {} |\n",
                record.timestamp,
                record.adapter,
                record.version,
                record.files.len(),
                record.wrote_files,
                record.accepted_hunks,
                record.elapsed_ms
            ));
        }
        output
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub adapter: String,
    pub cwd: PathBuf,
    pub status: String,
    pub output_tail: String,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: Vec<Session>,
}
impl SessionStore {
    pub fn save(&self, path: &Path) -> Result<(), AiError> {
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self, AiError> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }
}

#[derive(Debug, Error)]
pub enum AiError {
    #[error("AI I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("AI JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("AI base hash mismatch for {0}")]
    BaseMismatch(PathBuf),
    #[error("AI patch is not approved")]
    NotApproved,
}

pub fn content_hash(content: &[u8]) -> String {
    let mut hash: u64 = 14695981039346656037;
    for byte in content {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{hash:016x}")
}

pub struct DiffGate;
impl DiffGate {
    pub fn verify_bases(
        expected: &BTreeMap<PathBuf, String>,
        actual: &BTreeMap<PathBuf, Vec<u8>>,
    ) -> Result<(), AiError> {
        for (path, hash) in expected {
            let Some(bytes) = actual.get(path) else {
                return Err(AiError::BaseMismatch(path.clone()));
            };
            if content_hash(bytes) != *hash {
                return Err(AiError::BaseMismatch(path.clone()));
            }
        }
        Ok(())
    }
}

pub fn builtin_templates() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("explain", "Explain the selected code and its trade-offs."),
        (
            "refactor",
            "Refactor the selected code while preserving behavior.",
        ),
        ("tests", "Add focused tests for the selected code."),
        ("bug", "Find and fix the reported bug; propose a patch."),
        ("review", "Review the change and list concrete risks."),
        ("commit", "Generate a concise commit message."),
        ("changelog", "Generate a change summary for users."),
    ])
}

pub fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fr_ai01_adapters_have_stable_contracts_and_argv_invocations() {
        for adapter in BuiltinAdapter::all() {
            let invocation = adapter.build_invocation(&AgentRequest {
                prompt: "hi".into(),
                cwd: ".".into(),
                timeout_secs: 300,
            });
            assert_eq!(invocation.program, PathBuf::from(adapter.executable()));
            assert!(adapter.capabilities().headless);
        }
    }
    #[test]
    fn fr_ai08_json_events_are_tolerant_and_unknown_events_are_ignored() {
        assert_eq!(
            parse_json_event(r#"{"type":"text","text":"hi"}"#),
            Some(AgentEvent::TextDelta("hi".into()))
        );
        assert!(parse_json_event(r#"{"type":"future","x":1}"#).is_none());
    }
    #[test]
    fn fr_ai10_sensitive_context_is_excluded() {
        assert!(is_sensitive(Path::new(".env")));
        assert!(is_sensitive(Path::new(".ssh/id_rsa")));
        assert!(!is_sensitive(Path::new("src/main.rs")));
    }
    #[test]
    fn fr_ai05_diff_gate_rejects_changed_base() {
        let mut expected = BTreeMap::new();
        expected.insert(PathBuf::from("a.rs"), content_hash(b"old"));
        let mut actual = BTreeMap::new();
        actual.insert(PathBuf::from("a.rs"), b"new".to_vec());
        assert!(matches!(
            DiffGate::verify_bases(&expected, &actual),
            Err(AiError::BaseMismatch(_))
        ));
    }
    #[test]
    fn fr_ai12_templates_are_named_and_nonempty() {
        assert_eq!(builtin_templates().len(), 7);
        assert!(builtin_templates().values().all(|value| !value.is_empty()));
    }

    #[test]
    fn fr_ai06_session_metadata_and_ai11_audit_never_store_source_text() {
        let mut log = AuditLog::default();
        log.append(AuditRecord {
            timestamp: unix_seconds(),
            adapter: "codex".into(),
            version: "1".into(),
            files: vec![("src/main.rs".into(), 42)],
            wrote_files: false,
            accepted_hunks: 0,
            elapsed_ms: 12,
        });
        let markdown = log.to_markdown();
        assert!(!markdown.contains("src/main.rs"));
        assert!(markdown.contains("codex"));
        let spec = CustomAdapterSpec {
            id: "local".into(),
            display_name: "Local".into(),
            executable: "/opt/local-agent".into(),
            default_args: vec!["--json".into()],
            headless: true,
            install_command: None,
            minimum_version: None,
        };
        assert_eq!(
            spec.invocation(&AgentRequest {
                prompt: "x".into(),
                cwd: ".".into(),
                timeout_secs: 1
            })
            .args,
            vec!["--json"]
        );
    }
}
