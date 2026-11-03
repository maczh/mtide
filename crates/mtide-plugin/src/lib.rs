//! Safe process-plugin contracts: manifests, permissions, JSON-RPC framing,
//! lifecycle state, and ANSI-free widget trees.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Permission {
    WorkspaceRead,
    WorkspaceWrite,
    Execute,
    Network,
    Clipboard,
    Environment,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub entry: String,
    pub host: String,
    pub permissions: BTreeSet<Permission>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub license: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PluginError {
    #[error("plugin manifest field is missing: {0}")]
    MissingField(&'static str),
    #[error("plugin permission denied: {0:?}")]
    PermissionDenied(Permission),
    #[error("plugin path is outside workspace")]
    OutsideWorkspace,
    #[error("plugin RPC frame is invalid: {0}")]
    InvalidRpc(String),
    #[error("plugin output contains terminal control characters")]
    AnsiOutput,
    #[error("plugin manifest I/O failed: {0}")]
    Io(String),
    #[error("plugin manifest is invalid: {0}")]
    InvalidManifest(String),
}

impl Manifest {
    pub fn load(path: &Path, host_version: &str) -> Result<Self, PluginError> {
        let bytes = fs::read(path).map_err(|error| PluginError::Io(error.to_string()))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .map_err(|error| PluginError::InvalidManifest(error.to_string()))?;
        manifest.validate(host_version)?;
        Ok(manifest)
    }
    pub fn validate(&self, host_version: &str) -> Result<(), PluginError> {
        if self.name.trim().is_empty() {
            return Err(PluginError::MissingField("name"));
        }
        if self.version.trim().is_empty() {
            return Err(PluginError::MissingField("version"));
        }
        if self.entry.trim().is_empty() {
            return Err(PluginError::MissingField("entry"));
        }
        if self.host.trim().is_empty() {
            return Err(PluginError::MissingField("host"));
        }
        if self.permissions.is_empty() {
            return Err(PluginError::MissingField("permissions"));
        }
        if !host_version.starts_with(&self.host) && self.host != "*" {
            return Err(PluginError::InvalidRpc(format!(
                "host {host_version} is not compatible with {}",
                self.host
            )));
        }
        Ok(())
    }
}

pub fn discover_manifests(directory: &Path, host_version: &str) -> Vec<Manifest> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .map(|path| {
            if path.is_dir() {
                path.join("manifest.json")
            } else {
                path
            }
        })
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("manifest.json"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| Manifest::load(&path, host_version).ok())
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StyleToken {
    Default,
    Muted,
    Focus,
    Warning,
    Danger,
    Success,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WidgetNode {
    Text {
        text: String,
        style: StyleToken,
    },
    List {
        items: Vec<WidgetNode>,
        selected: Option<usize>,
    },
    Input {
        value: String,
        placeholder: Option<String>,
    },
    Button {
        label: String,
        command: String,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Progress {
        ratio: f32,
        label: String,
    },
    VStack(Vec<WidgetNode>),
    HStack(Vec<WidgetNode>),
}

impl WidgetNode {
    pub fn sanitize(self) -> Result<Self, PluginError> {
        fn clean(text: String) -> Result<String, PluginError> {
            // Escape sequences are the documented hazard, but bare control
            // characters (carriage return, bell, CSI in 8-bit form) can corrupt
            // the grid just as easily, so only newline and tab survive.
            if text.chars().any(|character| {
                (character.is_control() && character != '\n' && character != '\t')
                    || character == '\u{9b}'
            }) {
                Err(PluginError::AnsiOutput)
            } else {
                Ok(text)
            }
        }
        match self {
            Self::Text { text, style } => Ok(Self::Text {
                text: clean(text)?,
                style,
            }),
            Self::Input { value, placeholder } => Ok(Self::Input {
                value: clean(value)?,
                placeholder: placeholder.map(clean).transpose()?,
            }),
            Self::Button { label, command } => Ok(Self::Button {
                label: clean(label)?,
                command: clean(command)?,
            }),
            Self::Progress { ratio, label } => Ok(Self::Progress {
                ratio: ratio.clamp(0.0, 1.0),
                label: clean(label)?,
            }),
            Self::List { items, selected } => Ok(Self::List {
                items: items
                    .into_iter()
                    .map(Self::sanitize)
                    .collect::<Result<_, _>>()?,
                selected,
            }),
            Self::Table { headers, rows } => Ok(Self::Table {
                headers: headers.into_iter().map(clean).collect::<Result<_, _>>()?,
                rows: rows
                    .into_iter()
                    .map(|row| row.into_iter().map(clean).collect())
                    .collect::<Result<_, _>>()?,
            }),
            Self::VStack(items) => Ok(Self::VStack(
                items
                    .into_iter()
                    .map(Self::sanitize)
                    .collect::<Result<_, _>>()?,
            )),
            Self::HStack(items) => Ok(Self::HStack(
                items
                    .into_iter()
                    .map(Self::sanitize)
                    .collect::<Result<_, _>>()?,
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

pub fn encode_rpc(request: &RpcRequest) -> Result<Vec<u8>, PluginError> {
    let mut bytes =
        serde_json::to_vec(request).map_err(|error| PluginError::InvalidRpc(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode_rpc(line: &[u8]) -> Result<RpcRequest, PluginError> {
    serde_json::from_slice(line).map_err(|error| PluginError::InvalidRpc(error.to_string()))
}

#[derive(Clone, Debug)]
pub struct PermissionBroker {
    workspace: PathBuf,
    granted: BTreeSet<Permission>,
    violations: u8,
}

impl PermissionBroker {
    pub fn new(workspace: impl Into<PathBuf>, granted: BTreeSet<Permission>) -> Self {
        Self {
            workspace: workspace.into(),
            granted,
            violations: 0,
        }
    }
    pub fn check(&mut self, permission: Permission) -> Result<(), PluginError> {
        if self.granted.contains(&permission) {
            Ok(())
        } else {
            self.violations = self.violations.saturating_add(1);
            Err(PluginError::PermissionDenied(permission))
        }
    }
    pub fn check_path(&mut self, permission: Permission, path: &Path) -> Result<(), PluginError> {
        self.check(permission)?;
        // Resolve `.` and `..` lexically before the boundary check: without this
        // a plugin could reach outside the workspace with `<workspace>/../x`.
        let candidate = normalize_path(path);
        let workspace = normalize_path(&self.workspace);
        if candidate == workspace || candidate.starts_with(&workspace) {
            Ok(())
        } else {
            Err(PluginError::OutsideWorkspace)
        }
    }
    pub fn violations(&self) -> u8 {
        self.violations
    }
    pub fn should_disable(&self) -> bool {
        self.violations >= 3
    }
}

/// Lexical path normalisation: drops `.`, folds `..` and collapses repeated
/// separators without touching the file system.
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Installed,
    Enabled,
    Running,
    Crashed,
    Disabled,
    Uninstalled,
}

#[derive(Clone, Debug)]
pub struct PluginState {
    pub manifest: Manifest,
    pub lifecycle: Lifecycle,
    pub restart_count: u8,
    pub development: bool,
}
impl PluginState {
    pub fn crash(&mut self) {
        self.lifecycle = Lifecycle::Crashed;
        self.restart_count = self.restart_count.saturating_add(1);
        if self.restart_count > 3 {
            self.lifecycle = Lifecycle::Disabled;
        }
    }
    pub fn restart(&mut self) -> bool {
        if self.restart_count <= 3 && self.lifecycle == Lifecycle::Crashed {
            self.lifecycle = Lifecycle::Running;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        Manifest {
            name: "example".into(),
            version: "1.0".into(),
            entry: "plugin".into(),
            host: "1".into(),
            permissions: [Permission::WorkspaceRead].into(),
            extensions: vec!["command".into()],
            author: "test".into(),
            license: "MIT".into(),
        }
    }
    #[test]
    fn fr_pl01_manifest_rejects_missing_permissions_and_accepts_valid_metadata() {
        let mut value = manifest();
        assert!(value.validate("1.2").is_ok());
        value.permissions.clear();
        assert_eq!(
            value.validate("1.2"),
            Err(PluginError::MissingField("permissions"))
        );
    }
    #[test]
    fn fr_pl04_widget_sanitizes_and_rejects_ansi() {
        assert!(WidgetNode::Text {
            text: "\x1b[31mred".into(),
            style: StyleToken::Danger
        }
        .sanitize()
        .is_err());
        assert!(WidgetNode::Text {
            text: "safe".into(),
            style: StyleToken::Default
        }
        .sanitize()
        .is_ok());
    }
    #[test]
    fn fr_pl03_broker_enforces_permissions_and_workspace_boundary() {
        let mut broker = PermissionBroker::new("/workspace", [Permission::WorkspaceRead].into());
        assert!(broker
            .check_path(
                Permission::WorkspaceRead,
                Path::new("/workspace/src/main.rs")
            )
            .is_ok());
        assert!(broker.check(Permission::Execute).is_err());
        assert!(broker.check(Permission::Execute).is_err());
        assert!(broker.check(Permission::Execute).is_err());
        assert!(broker.should_disable());
    }
    #[test]
    fn fr_pl02_rpc_round_trip_and_pl06_crash_recovery() {
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "panel/render".into(),
            params: serde_json::json!({}),
        };
        let encoded = encode_rpc(&request).expect("encode");
        assert_eq!(
            decode_rpc(&encoded[..encoded.len() - 1]).expect("decode"),
            request
        );
        let mut state = PluginState {
            manifest: manifest(),
            lifecycle: Lifecycle::Running,
            restart_count: 0,
            development: false,
        };
        state.crash();
        assert!(state.restart());
    }

    #[test]
    fn fr_pl03_broker_rejects_parent_directory_escapes() {
        let mut broker = PermissionBroker::new("/workspace", [Permission::WorkspaceRead].into());
        assert_eq!(
            broker.check_path(
                Permission::WorkspaceRead,
                Path::new("/workspace/../etc/passwd")
            ),
            Err(PluginError::OutsideWorkspace)
        );
        assert_eq!(
            broker.check_path(
                Permission::WorkspaceRead,
                Path::new("/workspace-evil/secret")
            ),
            Err(PluginError::OutsideWorkspace)
        );
        assert!(broker
            .check_path(
                Permission::WorkspaceRead,
                Path::new("/workspace/src/./main.rs")
            )
            .is_ok());
    }

    #[test]
    fn fr_pl04_widget_sanitize_blocks_all_control_characters() {
        for text in ["carriage\rreturn", "bell\u{7}", "eight-bit\u{9b}31m"] {
            assert!(
                WidgetNode::Text {
                    text: text.into(),
                    style: StyleToken::Default
                }
                .sanitize()
                .is_err(),
                "{text:?} must be rejected"
            );
        }
        assert!(WidgetNode::Text {
            text: "multi\nline\ttab".into(),
            style: StyleToken::Default
        }
        .sanitize()
        .is_ok());
    }
    #[test]
    fn fr_pl05_manifest_discovery_skips_invalid_plugins() {
        let root = std::env::temp_dir().join(format!("mtide-plugin-{}", std::process::id()));
        let valid = root.join("good");
        let invalid = root.join("bad");
        fs::create_dir_all(&valid).expect("create valid plugin");
        fs::create_dir_all(&invalid).expect("create invalid plugin");
        fs::write(
            valid.join("manifest.json"),
            serde_json::to_vec(&manifest()).expect("serialize manifest"),
        )
        .expect("write valid manifest");
        fs::write(invalid.join("manifest.json"), b"not-json").expect("write invalid manifest");
        assert_eq!(discover_manifests(&root, "1.0").len(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
