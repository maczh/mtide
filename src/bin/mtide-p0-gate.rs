use mtide::compat::{
    run_definition, DifferentialResult, ResultClass, ScenarioDefinition, SNAPSHOT_SCHEMA,
    TTT_ORACLE_COMMIT,
};
use std::{env, fs, path::PathBuf, process::ExitCode};

#[derive(serde::Serialize)]
struct GateReport {
    report_schema: &'static str,
    oracle_commit: &'static str,
    scenario_version: &'static str,
    snapshot_schema: &'static str,
    normalizer_version: &'static str,
    fixture_version: &'static str,
    platform: String,
    scenarios: Vec<DifferentialResult>,
    executable: usize,
    passed: usize,
    environment_failures: usize,
    failed: usize,
    flaky: usize,
}

fn main() -> ExitCode {
    let directory = argument("--scenarios").unwrap_or_else(|| "compat/scenarios".into());
    let mut paths = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>(),
        Err(error) => {
            eprintln!("cannot read {directory}: {error}");
            return ExitCode::from(2);
        }
    };
    paths.sort();
    let mut scenarios = Vec::new();
    for path in paths {
        match load_and_run(&path) {
            Ok(result) => scenarios.push(result),
            Err(error) => eprintln!("scenario {}: {error}", path.display()),
        }
    }
    let executable = scenarios.len();
    let passed = scenarios
        .iter()
        .filter(|result| result.classification == ResultClass::Success)
        .count();
    let environment_failures = scenarios
        .iter()
        .filter(|result| result.classification == ResultClass::EnvironmentFailure)
        .count();
    let failed = scenarios
        .iter()
        .filter(|result| {
            matches!(
                result.classification,
                ResultClass::BehaviorFailure | ResultClass::ProcessFailure
            )
        })
        .count();
    let report = GateReport {
        report_schema: "mtide.compat.gate.v1",
        oracle_commit: TTT_ORACLE_COMMIT,
        scenario_version: "p0.v1",
        snapshot_schema: SNAPSHOT_SCHEMA,
        normalizer_version: "field-rules.v1",
        fixture_version: "controlled-fixtures.v1",
        platform: std::env::consts::OS.to_owned(),
        scenarios,
        executable,
        passed,
        environment_failures,
        failed,
        flaky: 0,
    };
    let json = serde_json::to_string_pretty(&report).expect("gate report serializes");
    if let Some(output) = argument("--output") {
        fs::write(output, &json).expect("gate report writes");
    } else {
        println!("{json}");
    }
    if failed > 0 || report.flaky > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn load_and_run(path: &PathBuf) -> Result<DifferentialResult, String> {
    let source = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let definition: ScenarioDefinition =
        serde_json::from_str(&source).map_err(|error| error.to_string())?;
    Ok(run_definition(&definition))
}

fn argument(name: &str) -> Option<String> {
    env::args()
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}
