//! Reproducible compatibility scenarios shared by the P0 differential harness.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};

pub const TTT_ORACLE_COMMIT: &str = "afa48edc0b35539423062bafef747dca610fbd5b";
pub const MANIFEST_SCHEMA: &str = "mtide.compat.manifest.v1";
pub const SNAPSHOT_SCHEMA: &str = "mtide.compat.snapshot.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixtureEvent {
    pub kind: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixtureLog {
    pub events: Vec<FixtureEvent>,
}

pub trait ClockPort {
    fn now_ms(&self) -> u64;
    fn advance(&mut self, duration: Duration);
}

pub trait SchedulerPort {
    fn schedule(&mut self, clock: &dyn ClockPort, delay: Duration) -> u64;
    fn cancel(&mut self, id: u64) -> bool;
}

pub trait FileSystemPort {
    fn write_file(&mut self, path: &Path, contents: &str) -> Result<(), String>;
    fn read_file(&self, path: &Path) -> Result<String, String>;
    fn rename_file(&mut self, from: &Path, to: &Path) -> Result<(), String>;
}

pub trait ProcessPort {
    fn stdout(&self) -> &str;
    fn cancel(&mut self);
    fn is_cancelled(&self) -> bool;
}

pub trait ClipboardPort {
    fn read_text(&mut self) -> Result<String, String>;
}

impl FixtureLog {
    pub fn record(&mut self, kind: impl Into<String>, value: serde_json::Value) {
        self.events.push(FixtureEvent {
            kind: kind.into(),
            value,
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualClock {
    now_ms: u64,
    pub log: FixtureLog,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            now_ms: 0,
            log: FixtureLog::default(),
        }
    }
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }
    pub fn advance(&mut self, duration: Duration) {
        self.now_ms = self.now_ms.saturating_add(duration.as_millis() as u64);
        self.log
            .record("clock.advance", serde_json::json!({"now_ms": self.now_ms}));
    }
}

impl ClockPort for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms()
    }
    fn advance(&mut self, duration: Duration) {
        self.advance(duration);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualScheduler {
    next_id: u64,
    timers: BTreeMap<u64, u64>,
    pub log: FixtureLog,
}

impl ManualScheduler {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            timers: BTreeMap::new(),
            log: FixtureLog::default(),
        }
    }
    pub fn schedule(&mut self, clock: &ManualClock, delay: Duration) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.timers
            .insert(id, clock.now_ms().saturating_add(delay.as_millis() as u64));
        self.log
            .record("timer.schedule", serde_json::json!({"id": id}));
        id
    }
    pub fn due(&self, clock: &ManualClock, id: u64) -> bool {
        self.timers
            .get(&id)
            .is_some_and(|deadline| *deadline <= clock.now_ms())
    }
    pub fn cancel(&mut self, id: u64) -> bool {
        self.timers.remove(&id).is_some()
    }
}

impl SchedulerPort for ManualScheduler {
    fn schedule(&mut self, clock: &dyn ClockPort, delay: Duration) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.timers
            .insert(id, clock.now_ms().saturating_add(delay.as_millis() as u64));
        self.log
            .record("timer.schedule", serde_json::json!({"id": id}));
        id
    }
    fn cancel(&mut self, id: u64) -> bool {
        self.cancel(id)
    }
}

impl Default for ManualScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileFixture {
    root: PathBuf,
    files: BTreeMap<PathBuf, String>,
    pub log: FixtureLog,
}

impl FileFixture {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            files: BTreeMap::new(),
            log: FixtureLog::default(),
        }
    }
    pub fn write(
        &mut self,
        path: impl AsRef<Path>,
        contents: impl Into<String>,
    ) -> Result<(), String> {
        let path = self.scoped(path)?;
        self.files.insert(path.clone(), contents.into());
        self.log
            .record("fs.write", serde_json::json!({"path": path}));
        Ok(())
    }
    pub fn read(&self, path: impl AsRef<Path>) -> Result<&str, String> {
        let path = self.scoped(path)?;
        self.files
            .get(&path)
            .map(String::as_str)
            .ok_or_else(|| "not found".into())
    }
    pub fn rename(&mut self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), String> {
        let from = self.scoped(from)?;
        let to = self.scoped(to)?;
        let contents = self
            .files
            .remove(&from)
            .ok_or_else(|| "not found".to_owned())?;
        self.files.insert(to.clone(), contents);
        self.log
            .record("fs.rename", serde_json::json!({"from": from, "to": to}));
        Ok(())
    }
    fn scoped(&self, path: impl AsRef<Path>) -> Result<PathBuf, String> {
        let path = path.as_ref();
        if path.is_absolute() || path.components().any(|part| part.as_os_str() == "..") {
            return Err("path outside fixture root".into());
        }
        Ok(self.root.join(path))
    }
}

impl FileSystemPort for FileFixture {
    fn write_file(&mut self, path: &Path, contents: &str) -> Result<(), String> {
        self.write(path, contents)
    }
    fn read_file(&self, path: &Path) -> Result<String, String> {
        self.read(path).map(str::to_owned)
    }
    fn rename_file(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        self.rename(from, to)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessFixture {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub log: FixtureLog,
}

impl ProcessFixture {
    pub fn new(stdout: impl Into<String>, exit_code: i32) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(exit_code),
            cancelled: false,
            log: FixtureLog::default(),
        }
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.log.record("process.cancel", serde_json::Value::Null);
    }
    pub fn bounded_output(&self, max_bytes: usize) -> &str {
        let end = self
            .stdout
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= max_bytes)
            .last()
            .unwrap_or(0);
        &self.stdout[..end]
    }
}

impl ProcessPort for ProcessFixture {
    fn stdout(&self) -> &str {
        &self.stdout
    }
    fn cancel(&mut self) {
        self.cancel();
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardFixture {
    text: Option<String>,
    failure: Option<String>,
    pub log: FixtureLog,
}

impl ClipboardFixture {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            failure: None,
            log: FixtureLog::default(),
        }
    }
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            text: None,
            failure: Some(message.into()),
            log: FixtureLog::default(),
        }
    }
    pub fn read(&mut self) -> Result<&str, String> {
        self.log.record("clipboard.read", serde_json::Value::Null);
        match (&self.text, &self.failure) {
            (Some(text), _) => Ok(text),
            (_, Some(error)) => Err(error.clone()),
            _ => Err("clipboard empty".into()),
        }
    }
}

impl ClipboardPort for ClipboardFixture {
    fn read_text(&mut self) -> Result<String, String> {
        self.read().map(str::to_owned)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenarioManifest {
    pub schema: String,
    pub scenario_id: String,
    pub oracle_commit: String,
    pub platform: String,
    pub terminal: TerminalSize,
    pub locale: String,
    pub environment_fingerprint: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl ScenarioManifest {
    pub fn new(scenario_id: impl Into<String>, terminal: TerminalSize) -> Self {
        let environment = deterministic_environment();
        let environment_fingerprint = fingerprint(&environment);
        Self {
            schema: MANIFEST_SCHEMA.to_owned(),
            scenario_id: scenario_id.into(),
            oracle_commit: TTT_ORACLE_COMMIT.to_owned(),
            platform: env::consts::OS.to_owned(),
            terminal,
            locale: "C".to_owned(),
            environment_fingerprint,
            environment,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenarioCommand {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenarioDefinition {
    pub scenario_id: String,
    pub terminal: TerminalSize,
    pub oracle: ScenarioCommand,
    pub candidate: ScenarioCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliTargetScenario {
    pub scenario_id: String,
    pub target: String,
    pub kind: CliTargetKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CliTargetKind {
    File,
    Directory,
    Stdin,
    Position,
    Workspace,
    Invalid,
}

pub fn cli_target_scenarios() -> Vec<CliTargetScenario> {
    vec![
        CliTargetScenario {
            scenario_id: "cli-file".into(),
            target: "fixture/main.rs".into(),
            kind: CliTargetKind::File,
        },
        CliTargetScenario {
            scenario_id: "cli-directory".into(),
            target: "fixture".into(),
            kind: CliTargetKind::Directory,
        },
        CliTargetScenario {
            scenario_id: "cli-stdin".into(),
            target: "-".into(),
            kind: CliTargetKind::Stdin,
        },
        CliTargetScenario {
            scenario_id: "cli-position".into(),
            target: "fixture/main.rs:3:5".into(),
            kind: CliTargetKind::Position,
        },
        CliTargetScenario {
            scenario_id: "cli-workspace".into(),
            target: "fixture/project.ttt".into(),
            kind: CliTargetKind::Workspace,
        },
        CliTargetScenario {
            scenario_id: "cli-invalid-target".into(),
            target: "fixture/missing.rs".into(),
            kind: CliTargetKind::Invalid,
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResultClass {
    Success,
    BehaviorFailure,
    ProcessFailure,
    EnvironmentFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenarioResult {
    pub scenario_id: String,
    pub implementation: String,
    pub class: ResultClass,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DifferentialResult {
    pub manifest: ScenarioManifest,
    pub oracle: ScenarioResult,
    pub candidate: ScenarioResult,
    pub classification: ResultClass,
    pub oracle_snapshot: SemanticSnapshot,
    pub candidate_snapshot: SemanticSnapshot,
    pub oracle_normalized: SemanticSnapshot,
    pub candidate_normalized: SemanticSnapshot,
    pub snapshot_diffs: Vec<SnapshotDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticSnapshot {
    pub schema: String,
    pub meta: serde_json::Value,
    pub screen: serde_json::Value,
    pub app: serde_json::Value,
    pub side_effects: serde_json::Value,
    pub protocol: serde_json::Value,
    pub lifecycle: serde_json::Value,
}

impl SemanticSnapshot {
    pub fn empty() -> Self {
        let empty = serde_json::Value::Object(Default::default());
        Self {
            schema: SNAPSHOT_SCHEMA.to_owned(),
            meta: empty.clone(),
            screen: empty.clone(),
            app: empty.clone(),
            side_effects: empty.clone(),
            protocol: empty.clone(),
            lifecycle: empty,
        }
    }

    fn layer(&self, name: &str) -> Option<&serde_json::Value> {
        match name {
            "meta" => Some(&self.meta),
            "screen" => Some(&self.screen),
            "app" => Some(&self.app),
            "side_effects" => Some(&self.side_effects),
            "protocol" => Some(&self.protocol),
            "lifecycle" => Some(&self.lifecycle),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NormalizationRule {
    Replace {
        path: String,
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDiff {
    pub path: String,
    pub oracle: Option<serde_json::Value>,
    pub candidate: Option<serde_json::Value>,
    pub scenario_id: String,
    pub platform: String,
    pub class: ResultClass,
}

pub fn normalize_snapshot(
    snapshot: &SemanticSnapshot,
    rules: &[NormalizationRule],
) -> SemanticSnapshot {
    let mut normalized = snapshot.clone();
    for rule in rules {
        let NormalizationRule::Replace { path, value } = rule;
        if let Some((layer, json_path)) = path.split_once('.') {
            if let Some(target) = normalized.layer_mut(layer) {
                replace_at_path(target, json_path, value.clone());
            }
        }
    }
    normalized
}

pub fn diff_snapshots(
    oracle: &SemanticSnapshot,
    candidate: &SemanticSnapshot,
    manifest: &ScenarioManifest,
    class: ResultClass,
) -> Vec<SnapshotDiff> {
    let layers = [
        "meta",
        "screen",
        "app",
        "side_effects",
        "protocol",
        "lifecycle",
    ];
    let mut diffs = Vec::new();
    for layer in layers {
        if let (Some(left), Some(right)) = (oracle.layer(layer), candidate.layer(layer)) {
            diff_value(layer, left, right, manifest, &class, &mut diffs);
        }
    }
    diffs
}

impl SemanticSnapshot {
    fn layer_mut(&mut self, name: &str) -> Option<&mut serde_json::Value> {
        match name {
            "meta" => Some(&mut self.meta),
            "screen" => Some(&mut self.screen),
            "app" => Some(&mut self.app),
            "side_effects" => Some(&mut self.side_effects),
            "protocol" => Some(&mut self.protocol),
            "lifecycle" => Some(&mut self.lifecycle),
            _ => None,
        }
    }
}

fn replace_at_path(target: &mut serde_json::Value, path: &str, replacement: serde_json::Value) {
    let mut current = target;
    let parts: Vec<&str> = path.split('.').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let Some(next) = current.get_mut(*part) else {
            return;
        };
        current = next;
    }
    if let Some(last) = parts.last() {
        if let Some(slot) = current.get_mut(*last) {
            *slot = replacement;
        }
    }
}

fn diff_value(
    path: &str,
    oracle: &serde_json::Value,
    candidate: &serde_json::Value,
    manifest: &ScenarioManifest,
    class: &ResultClass,
    diffs: &mut Vec<SnapshotDiff>,
) {
    match (oracle, candidate) {
        (serde_json::Value::Object(left), serde_json::Value::Object(right)) => {
            let keys = left
                .keys()
                .chain(right.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                diff_value(
                    &format!("{path}.{key}"),
                    left.get(key).unwrap_or(&serde_json::Value::Null),
                    right.get(key).unwrap_or(&serde_json::Value::Null),
                    manifest,
                    class,
                    diffs,
                );
            }
        }
        (serde_json::Value::Array(left), serde_json::Value::Array(right)) if left == right => {}
        _ if oracle == candidate => {}
        _ => diffs.push(SnapshotDiff {
            path: path.to_owned(),
            oracle: Some(oracle.clone()),
            candidate: Some(candidate.clone()),
            scenario_id: manifest.scenario_id.clone(),
            platform: manifest.platform.clone(),
            class: class.clone(),
        }),
    }
}

pub fn run_definition(definition: &ScenarioDefinition) -> DifferentialResult {
    let manifest = ScenarioManifest::new(&definition.scenario_id, definition.terminal);
    let oracle = run_scenario(&manifest, "oracle", &definition.oracle);
    let candidate = run_scenario(&manifest, "candidate", &definition.candidate);
    let oracle_snapshot = snapshot_from_result(&oracle);
    let candidate_snapshot = snapshot_from_result(&candidate);
    let oracle_normalized = normalize_snapshot(&oracle_snapshot, &[]);
    let candidate_normalized = normalize_snapshot(&candidate_snapshot, &[]);
    let mut classification = compare_results(&oracle, &candidate);
    let snapshot_diffs = diff_snapshots(
        &oracle_normalized,
        &candidate_normalized,
        &manifest,
        ResultClass::BehaviorFailure,
    );
    if classification == ResultClass::Success && !snapshot_diffs.is_empty() {
        classification = ResultClass::BehaviorFailure;
    }
    DifferentialResult {
        manifest,
        oracle,
        candidate,
        classification,
        oracle_snapshot,
        candidate_snapshot,
        oracle_normalized,
        candidate_normalized,
        snapshot_diffs,
    }
}

pub fn snapshot_from_result(result: &ScenarioResult) -> SemanticSnapshot {
    let mut snapshot = SemanticSnapshot::empty();
    snapshot.meta = serde_json::json!({
        "scenario_id": result.scenario_id,
        "exit_code": result.exit_code,
    });
    snapshot.screen = serde_json::json!({"stdout": result.stdout});
    snapshot.protocol = serde_json::json!({"stderr": result.stderr});
    snapshot.lifecycle = serde_json::json!({"class": result.class});
    snapshot
}

pub fn compare_results(oracle: &ScenarioResult, candidate: &ScenarioResult) -> ResultClass {
    if oracle.class == ResultClass::EnvironmentFailure
        || candidate.class == ResultClass::EnvironmentFailure
    {
        return ResultClass::EnvironmentFailure;
    }
    if oracle.class != ResultClass::Success || candidate.class != ResultClass::Success {
        return ResultClass::ProcessFailure;
    }
    if oracle.exit_code != candidate.exit_code
        || oracle.stdout != candidate.stdout
        || oracle.stderr != candidate.stderr
    {
        ResultClass::BehaviorFailure
    } else {
        ResultClass::Success
    }
}

pub fn run_scenario(
    manifest: &ScenarioManifest,
    implementation: &str,
    command: &ScenarioCommand,
) -> ScenarioResult {
    let output = Command::new(&command.program)
        .args(&command.args)
        .envs(&manifest.environment)
        .env("COLUMNS", manifest.terminal.columns.to_string())
        .env("LINES", manifest.terminal.rows.to_string())
        .stdin(Stdio::null())
        .output();

    match output {
        Ok(output) => ScenarioResult {
            scenario_id: manifest.scenario_id.clone(),
            implementation: implementation.to_owned(),
            class: classify_exit(output.status),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => ScenarioResult {
            scenario_id: manifest.scenario_id.clone(),
            implementation: implementation.to_owned(),
            class: ResultClass::EnvironmentFailure,
            exit_code: None,
            stdout: String::new(),
            stderr: error.to_string(),
        },
    }
}

fn classify_exit(status: ExitStatus) -> ResultClass {
    if status.success() {
        ResultClass::Success
    } else {
        ResultClass::ProcessFailure
    }
}

fn deterministic_environment() -> BTreeMap<String, String> {
    [
        ("HOME", "/mtide-compat/home"),
        ("LC_ALL", "C"),
        ("LANG", "C"),
        ("TZ", "UTC"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

fn fingerprint(environment: &BTreeMap<String, String>) -> String {
    let canonical = environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{:x}", fnv1a(canonical.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_pins_oracle_and_stabilizes_environment() {
        let manifest = ScenarioManifest::new(
            "startup-exit",
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        assert_eq!(manifest.oracle_commit, TTT_ORACLE_COMMIT);
        assert_eq!(
            manifest.environment.get("HOME"),
            Some(&"/mtide-compat/home".to_owned())
        );
        assert_eq!(manifest.environment_fingerprint, "b025a56fe5e1c94c");
    }

    #[test]
    fn runner_executes_the_same_command_and_records_success() {
        let manifest = ScenarioManifest::new(
            "startup-exit",
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let command = ScenarioCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "printf ready".into()],
        };
        let result = run_scenario(&manifest, "candidate", &command);
        assert_eq!(result.class, ResultClass::Success);
        assert_eq!(result.stdout, "ready");
        assert_eq!(result.scenario_id, "startup-exit");
    }

    #[test]
    fn missing_program_is_an_environment_failure() {
        let manifest = ScenarioManifest::new(
            "startup-exit",
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let command = ScenarioCommand {
            program: "definitely-not-installed-mtide".into(),
            args: vec![],
        };
        assert_eq!(
            run_scenario(&manifest, "oracle", &command).class,
            ResultClass::EnvironmentFailure
        );
    }

    #[test]
    fn comparison_reports_behavior_failure_without_hiding_process_failures() {
        let oracle = ScenarioResult {
            scenario_id: "startup-exit".into(),
            implementation: "oracle".into(),
            class: ResultClass::Success,
            exit_code: Some(0),
            stdout: "oracle".into(),
            stderr: String::new(),
        };
        let mut candidate = oracle.clone();
        candidate.implementation = "candidate".into();
        candidate.stdout = "candidate".into();
        assert_eq!(
            compare_results(&oracle, &candidate),
            ResultClass::BehaviorFailure
        );
        candidate.class = ResultClass::ProcessFailure;
        assert_eq!(
            compare_results(&oracle, &candidate),
            ResultClass::ProcessFailure
        );
    }

    #[test]
    fn definition_runs_both_sides_and_keeps_the_manifest() {
        let definition = ScenarioDefinition {
            scenario_id: "startup-exit".into(),
            terminal: TerminalSize {
                columns: 80,
                rows: 24,
            },
            oracle: ScenarioCommand {
                program: "sh".into(),
                args: vec!["-c".into(), "printf same".into()],
            },
            candidate: ScenarioCommand {
                program: "sh".into(),
                args: vec!["-c".into(), "printf same".into()],
            },
        };
        let result = run_definition(&definition);
        assert_eq!(result.classification, ResultClass::Success);
        assert_eq!(result.oracle.implementation, "oracle");
        assert_eq!(result.candidate.implementation, "candidate");
        assert_eq!(result.manifest.scenario_id, "startup-exit");
    }

    #[test]
    fn normalization_only_replaces_explicit_dynamic_fields() {
        let mut oracle = SemanticSnapshot::empty();
        oracle.meta = serde_json::json!({"pid": 10, "command_id": "save", "dirty": true});
        let mut candidate = oracle.clone();
        candidate.meta["pid"] = serde_json::json!(20);
        candidate.meta["dirty"] = serde_json::json!(false);
        let rules = [NormalizationRule::Replace {
            path: "meta.pid".into(),
            value: serde_json::json!("<pid>"),
        }];
        let normalized_oracle = normalize_snapshot(&oracle, &rules);
        let normalized = normalize_snapshot(&candidate, &rules);
        let manifest = ScenarioManifest::new(
            "startup-exit",
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let diffs = diff_snapshots(
            &normalized_oracle,
            &normalized,
            &manifest,
            ResultClass::BehaviorFailure,
        );
        assert!(diffs.iter().any(|diff| diff.path == "meta.dirty"));
        assert!(!diffs.iter().any(|diff| diff.path == "meta.pid"));
        assert_eq!(normalized.meta["command_id"], "save");
    }

    #[test]
    fn all_six_layers_are_serialized_and_paths_are_auditable() {
        let mut oracle = SemanticSnapshot::empty();
        oracle.screen = serde_json::json!({"focus": "editor"});
        let mut candidate = oracle.clone();
        candidate.screen["focus"] = serde_json::json!("terminal");
        let manifest = ScenarioManifest::new(
            "startup-exit",
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let diffs = diff_snapshots(&oracle, &candidate, &manifest, ResultClass::BehaviorFailure);
        assert_eq!(diffs[0].path, "screen.focus");
        let encoded = serde_json::to_value(SemanticSnapshot::empty()).expect("snapshot JSON");
        for layer in [
            "meta",
            "screen",
            "app",
            "side_effects",
            "protocol",
            "lifecycle",
        ] {
            assert!(encoded.get(layer).is_some());
        }
    }

    #[test]
    fn manual_clock_scheduler_and_fixtures_are_replayable() {
        let mut clock = ManualClock::new();
        let mut scheduler = ManualScheduler::new();
        let timer = scheduler.schedule(&clock, Duration::from_millis(100));
        assert!(!scheduler.due(&clock, timer));
        clock.advance(Duration::from_millis(100));
        assert!(scheduler.due(&clock, timer));

        let mut files = FileFixture::new("/fixture");
        files.write("a.txt", "hello").expect("write");
        files.rename("a.txt", "b.txt").expect("rename");
        assert_eq!(files.read("b.txt"), Ok("hello"));
        assert!(files.read("../escape").is_err());

        let mut process = ProcessFixture::new("012345", 0);
        process.cancel();
        assert!(process.cancelled);
        assert_eq!(process.bounded_output(3), "012");
        let unicode_process = ProcessFixture::new("界面", 0);
        assert_eq!(unicode_process.bounded_output(1), "");

        let mut clipboard = ClipboardFixture::success("copied");
        assert_eq!(clipboard.read(), Ok("copied"));
        let mut failed_clipboard = ClipboardFixture::failure("denied");
        assert_eq!(failed_clipboard.read(), Err("denied".into()));
        assert!(!clock.log.events.is_empty());
        assert!(!files.log.events.is_empty());
    }

    #[test]
    fn cli_target_forms_have_stable_ids_and_fixture_side_effects() {
        let scenarios = cli_target_scenarios();
        assert_eq!(scenarios.len(), 6);
        assert_eq!(scenarios[0].scenario_id, "cli-file");
        assert_eq!(scenarios[1].scenario_id, "cli-directory");
        assert_eq!(scenarios[2].scenario_id, "cli-stdin");
        assert_eq!(scenarios[3].scenario_id, "cli-position");
        assert_eq!(scenarios[4].scenario_id, "cli-workspace");
        assert_eq!(scenarios[5].kind, CliTargetKind::Invalid);

        let mut files = FileFixture::new("/fixture");
        files.write("main.rs", "fn main() {}\n").expect("seed");
        files
            .write("main.rs", "fn main() { println!(\"saved\"); }\n")
            .expect("save");
        assert_eq!(
            files.read("main.rs").expect("read"),
            "fn main() { println!(\"saved\"); }\n"
        );
    }

    #[test]
    fn editor_semantics_keep_text_cursor_selection_and_dirty_state_visible() {
        let mut oracle = SemanticSnapshot::empty();
        oracle.app = serde_json::json!({
            "text": "界面",
            "cursor": {"line": 0, "column": 2},
            "selection": null,
            "dirty": true,
            "exit_code": 0
        });
        let mut candidate = oracle.clone();
        candidate.app["dirty"] = serde_json::json!(false);
        let manifest = ScenarioManifest::new(
            "editor-unicode-undo",
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let diffs = diff_snapshots(&oracle, &candidate, &manifest, ResultClass::BehaviorFailure);
        assert_eq!(
            diffs.iter().filter(|diff| diff.path == "app.dirty").count(),
            1
        );
        assert!(diffs.iter().all(|diff| diff.path != "app.text"));
    }

    #[test]
    fn plugin_permission_and_lifecycle_results_are_not_hidden() {
        let mut oracle = SemanticSnapshot::empty();
        oracle.protocol = serde_json::json!({"permission": "denied", "command": "fs.write"});
        oracle.lifecycle = serde_json::json!({"unloaded": true, "stale_callbacks": 0});
        let mut candidate = oracle.clone();
        candidate.protocol["permission"] = serde_json::json!("allowed");
        let manifest = ScenarioManifest::new(
            "plugin-permission-lifecycle",
            TerminalSize {
                columns: 120,
                rows: 40,
            },
        );
        let diffs = diff_snapshots(&oracle, &candidate, &manifest, ResultClass::BehaviorFailure);
        assert!(diffs.iter().any(|diff| diff.path == "protocol.permission"));
        assert!(diffs
            .iter()
            .all(|diff| diff.path != "lifecycle.stale_callbacks"));
    }
}
