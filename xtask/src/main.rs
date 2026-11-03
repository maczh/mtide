use mtide_core::{DocId, Document, Edit};
use std::process::Command;
use std::time::Instant;

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("{program}: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{program} {:?} failed with {status}", args))
}

fn main() -> Result<(), String> {
    match std::env::args().nth(1).as_deref() {
        Some("check") | None => {
            run("cargo", &["fmt", "--all", "--", "--check"])?;
            run(
                "cargo",
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--all-features",
                    "--",
                    "-D",
                    "warnings",
                ],
            )?;
            run("cargo", &["test", "--workspace"])?;
            check_dependencies()?;
        }
        Some("depcheck") => check_dependencies()?,
        Some("snapshot") => run(
            "cargo",
            &["test", "-p", "mtide-tui", "fr_p0_snapshot_dimensions"],
        )?,
        Some("bench") => bench()?,
        Some(command) => return Err(format!("unknown xtask command: {command}")),
    }
    Ok(())
}

fn bench() -> Result<(), String> {
    let status = Command::new("cargo")
        .args(["build", "-p", "mtide", "--release"])
        .status()
        .map_err(|error| format!("cargo build: {error}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}"));
    }

    let mut document = Document::new(DocId(7), &"x\n".repeat(1_000));
    let start = Instant::now();
    for _ in 0..1_000 {
        let position = 0;
        document
            .apply(&[Edit {
                range: position..position,
                insert: "# ".into(),
            }])
            .map_err(|error| format!("benchmark edit failed: {error}"))?;
    }
    println!("bench: 1,000 rope edits: {:?}", start.elapsed());

    let large_text = "x\n".repeat(1_000_000);
    let mut large_document = Document::new(DocId(8), &large_text);
    let middle = large_document
        .line_to_byte(500_000)
        .ok_or("million-line benchmark could not locate middle line")?;
    let start = Instant::now();
    large_document
        .apply(&[Edit {
            range: middle..middle,
            insert: "middle".into(),
        }])
        .map_err(|error| format!("million-line benchmark edit failed: {error}"))?;
    let elapsed = start.elapsed();
    println!("bench: million-line middle edit: {elapsed:?} (target <= 10ms)");
    if elapsed.as_millis() > 10 {
        return Err(format!(
            "million-line middle edit exceeded 10ms target: {elapsed:?}"
        ));
    }

    let start = Instant::now();
    let status = Command::new("cargo")
        .args(["test", "-p", "mtide-core", "--release", "--", "--nocapture"])
        .status()
        .map_err(|error| format!("cargo test: {error}"))?;
    if !status.success() {
        return Err(format!("core benchmark prerequisite failed with {status}"));
    }
    println!(
        "bench: mtide-core release test wall time: {:?}",
        start.elapsed()
    );
    Ok(())
}

fn check_dependencies() -> Result<(), String> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err("cargo metadata failed".into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("metadata JSON: {error}"))?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("metadata packages missing")?;
    let names = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for required in [
        "mtide-api",
        "mtide-proc",
        "mtide-core",
        "mtide-tui",
        "mtide-workspace",
        "mtide-search",
        "mtide-syntax",
        "mtide-vt",
        "mtide-term",
        "mtide-task",
        "mtide-git",
        "mtide-diff",
        "mtide-nav",
        "mtide-ai",
        "mtide-plugin",
        "mtide",
    ] {
        if !names.contains(required) {
            return Err(format!("required workspace crate missing: {required}"));
        }
    }
    let forbidden = [
        ("mtide-core", "mtide-tui"),
        ("mtide-core", "mtide-workspace"),
        ("mtide-tui", "mtide-core"),
        ("mtide-search", "mtide-core"),
    ];
    for package in packages {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        let dependencies = package["dependencies"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["name"].as_str())
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        for (source, target) in forbidden {
            if name == source && dependencies.contains(target) {
                return Err(format!(
                    "dependency direction violation: {source} -> {target}"
                ));
            }
        }
    }
    println!("dependency direction check: passed");
    Ok(())
}
