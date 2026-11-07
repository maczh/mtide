//! Reproducible compatibility scenarios shared by the P0 differential harness.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    process::{Command, ExitStatus, Stdio},
};

pub const TTT_ORACLE_COMMIT: &str = "afa48edc0b35539423062bafef747dca610fbd5b";
pub const MANIFEST_SCHEMA: &str = "mtide.compat.manifest.v1";
pub const SNAPSHOT_SCHEMA: &str = "mtide.compat.snapshot.v1";

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    let classification = compare_results(&oracle, &candidate);
    DifferentialResult {
        manifest,
        oracle,
        candidate,
        classification,
    }
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
}
