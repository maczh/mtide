# P0 compatibility scenarios

Run the complete gate from the repository root:

```bash
cargo run --bin mtide-p0-gate -- \
  --scenarios compat/scenarios \
  --output target/p0-compat-report.json
```

Before interpreting a compatibility result, verify the oracle checkout:

```bash
scripts/verify-ttt-baseline.sh /path/to/ttt
```

Each JSON file is a `ScenarioDefinition`. `oracle` and `candidate` receive the
same argument vector. The runner records the manifest, process result, raw and
normalized six-layer snapshots, and JSON-path differences.

Scenario metadata uses these optional fields:

- `capability`: stable product capability name used in coverage reports.
- `gui_terminal`: `true` when the result depends on a GUI terminal emulator.
- `dependency_class`: `none`, `external-process`, `network`, `gui-terminal`, or
  another explicit environment category.

The gate fails closed for malformed scenarios, empty scenario directories,
environment failures, process failures, behavior mismatches, and unclassified
flaky results. Environment and GUI-terminal failures remain separately
classified and must not be interpreted as product behavior equivalence.

The current runner is intentionally process-oriented. Full PTY screen capture,
real ttt checkout execution, and Linux/macOS/Windows matrix runs are release
validation steps; they must be performed in the corresponding environments.
