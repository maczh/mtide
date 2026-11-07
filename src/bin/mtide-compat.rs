use mtide::compat::{run_definition, ResultClass, ScenarioDefinition};
use std::{env, fs, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let Some(scenario_path) = option(&args, "--scenario") else {
        eprintln!("usage: mtide-compat --scenario PATH [--output PATH]");
        return ExitCode::from(2);
    };
    let source = match fs::read_to_string(scenario_path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot read scenario {scenario_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let definition: ScenarioDefinition = match serde_json::from_str(&source) {
        Ok(definition) => definition,
        Err(error) => {
            eprintln!("invalid scenario {scenario_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let result = run_definition(&definition);
    let json = match serde_json::to_string_pretty(&result) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("cannot encode result: {error}");
            return ExitCode::from(2);
        }
    };
    if let Some(output_path) = option(&args, "--output") {
        if let Err(error) = fs::write(output_path, &json) {
            eprintln!("cannot write result {output_path}: {error}");
            return ExitCode::from(2);
        }
    } else {
        println!("{json}");
    }
    if result.classification == ResultClass::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}
