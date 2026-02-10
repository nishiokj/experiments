# AgentLab (Prototype)

AgentLab is an experiment runner for agent/trainer systems with:
- dataset-driven trials (`tasks.jsonl`)
- explicit A/B variant plans
- isolation-aware execution (container/local modes)
- reproducible run artifacts under `.lab/runs/<run_id>/`
- analysis tables + report output
- provenance/debug bundle artifacts

## Architecture

Core execution shape:

```mermaid
flowchart LR
  A["experiment.yaml + tasks.jsonl"] --> B["Rust Runner (lab-runner)"]
  B --> C["Harness Process (CLI command)"]
  C --> D["trial_output.json / hooks / traces"]
  B --> E["Run Artifacts (.lab/runs/<run_id>)"]
  E --> F["Analysis Tables (JSONL)"]
  E --> G["Report (HTML)"]
  E --> H["Attestation + Debug Bundle"]
```

Mental model:
- Runner writes `trial_input.json` per trial.
- Harness consumes input and writes `trial_output.json`.
- Optional `harness_manifest.json` and `harness_events.jsonl` increase interpretability.
- Analysis and report are built from run artifacts, not ad-hoc logs.

---

## Rust CLI (Primary Runtime)

The Rust CLI (`lab-cli`) is the primary execution engine.

### Build

```bash
git clone <agentlab-repo>
cd <agentlab-repo>/rust
cargo build -p lab-cli --release
```

Use from a harness repo:

```bash
ln -sf /path/to/agentlab/rust/target/release/lab-cli ./lab
./lab --help
```

Prerequisites:
- Docker daemon running (for container mode)
- harness entry command exists in repo
- container image available or buildable

### First Run (Harness Repo)

1. Initialize:

```bash
./lab init --container
```

Trainer/deep-learning workload:

```bash
./lab init --container --workload-type trainer
```

2. Inspect resolved config:

```bash
./lab describe .lab/experiment.yaml
```

Optional knob registry:

```bash
./lab knobs-init
./lab knobs-validate
./lab describe .lab/experiment.yaml --overrides .lab/knobs/overrides.json
```

3. Dev run (runtime dependency install):

```bash
./lab run-dev .lab/experiment.yaml --overrides .lab/knobs/overrides.json --setup "<install command>"
```

`<install command>` examples:
- Bun: `bun install --frozen-lockfile`
- npm: `npm ci`
- pnpm: `pnpm install --frozen-lockfile`
- yarn: `yarn install --frozen-lockfile`
- uv: `uv sync --frozen`
- pip: `pip install -r requirements.txt`

4. Experiment run (strict network-none posture):

```bash
./lab run-experiment .lab/experiment.yaml --overrides .lab/knobs/overrides.json
```

### Command Semantics

- `run-dev`: effective network mode `full`, faster iteration.
- `run-experiment`: requires effective network mode `none`.
- `image-build`: build custom runtime image.
- `knobs-init`: writes `.lab/knobs/manifest.json` + `.lab/knobs/overrides.json`.
- `knobs-validate`: validates override IDs/types/ranges/options.
- `clean --init`: removes init files.
- `clean --runs`: removes run directories.

### JSON Machine Mode (SDK/UI Contract)

Core commands support `--json` and emit one JSON object:

```bash
./lab describe .lab/experiment.yaml --json
./lab run-dev .lab/experiment.yaml --overrides .lab/knobs/overrides.json --json
./lab run-experiment .lab/experiment.yaml --overrides .lab/knobs/overrides.json --json
./lab image-build .lab/experiment.yaml --json
./lab publish --run-dir .lab/runs/<run_id> --json
```

Success envelope:

```json
{ "ok": true, "command": "describe", "summary": { "...": "..." } }
```

Error envelope:

```json
{ "ok": false, "error": { "code": "command_failed", "message": "...", "details": {} } }
```

### Workload Types

Supported:
- `agent_harness`
- `trainer`

Set via init:

```bash
./lab init --workload-type trainer
```

Trainer output expectations:
- `trial_output.objective`: `name`, `value`, optional `direction`
- `trial_output.metrics`: secondary metrics (`train_loss`, `val_acc`, `wall_time_s`, etc.)
- optional `trial_output.checkpoints`

These flow to:
- `analysis/tables/trials.jsonl`
- `analysis/tables/metrics_long.jsonl`

### Pathing and Troubleshooting

Pathing:
- `runtime.harness.command` resolves from harness project root.
- If experiment is `.lab/experiment.yaml`, project root is parent of `.lab`.
- `describe` prints `harness_script_resolved` and `harness_script_exists`.

Common failures:
- `harness command file not found`:
  - fix `runtime.harness.command` in `.lab/experiment.yaml`
  - rerun `./lab describe .lab/experiment.yaml`
- dependency/native module errors in container:
  - use `run-dev` + proper `--setup` install command
  - or bake dependencies into image and use `run-experiment`
- missing trial output:
  - runner writes fallback `trial_output.json` with `harness_process_error`

### Analytics (DuckDB)

Per run tables:
- `.lab/runs/<run_id>/analysis/tables/trials.jsonl`
- `.lab/runs/<run_id>/analysis/tables/metrics_long.jsonl`
- `.lab/runs/<run_id>/analysis/tables/event_counts_by_trial.jsonl`
- `.lab/runs/<run_id>/analysis/tables/event_counts_by_variant.jsonl`
- `.lab/runs/<run_id>/analysis/tables/variant_summary.jsonl`
- `.lab/runs/<run_id>/analysis/tables/load_duckdb.sql`

Quick load:

```bash
cd .lab/runs/<run_id>/analysis
duckdb agentlab.duckdb < tables/load_duckdb.sql
```

---

## TypeScript SDK (`@agentlab/sdk`)

Local package path: `sdk`

```bash
cd sdk
npm install
npm run build
npm test
```

Example:

```ts
import { ExperimentBuilder, LabClient } from '@agentlab/sdk';

const client = new LabClient({ runnerBin: '/path/to/lab' });
const experiment = ExperimentBuilder.create('exp_1', 'Prompt A/B')
  .datasetJsonl('tasks.jsonl', { limit: 50 })
  .harnessCli(['node', './harness.js', 'run'], { integrationLevel: 'cli_events' })
  .baseline('base', { prompt: 'prompt:v1' })
  .addVariant('prompt_v2', { prompt: 'prompt:v2' })
  .build();

// Write to .lab/experiment.yaml, then:
const summary = await client.describe({ experiment: '.lab/experiment.yaml' });
const run = await client.runExperiment({ experiment: '.lab/experiment.yaml' });
console.log(summary.summary.total_trials, run.run.run_id);
```

Runner discovery order:
1. `runnerBin` option
2. `AGENTLAB_RUNNER_BIN`
3. default `lab`


