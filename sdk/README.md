# @agentlab/sdk

TypeScript SDK for defining experiments and driving the AgentLab Rust runner. Build experiment configs with a fluent API, execute them through a typed client, get structured JSON results.

## Install

```bash
npm install @agentlab/sdk
```

Local development:

```bash
cd sdk && npm install && npm run build && npm test
```

## Quick Start

```ts
import { ExperimentBuilder, LabClient, Metric } from '@agentlab/sdk';
import { writeFileSync, mkdirSync } from 'node:fs';

// 1. Define the experiment
const builder = ExperimentBuilder.create('prompt_ab', 'Prompt A/B Test')
  .description('Compare prompt v1 vs v2 on coding tasks')

  // Your dataset — path relative to the directory containing experiment.yaml.
  .datasetJsonl('./data/tasks.jsonl', {
    suiteId: 'coding_tasks',
    splitId: 'dev',
    limit: 50,
  })

  // Your harness — the command the runner invokes for EACH trial.
  // Path is relative to the project root (parent of .lab/).
  //
  //   Node:   ['node', './src/harness/run-trial.js']
  //   Python: ['python', '-m', 'my_agent.harness']
  //   Binary: ['./bin/evaluate']
  //
  // If this path is wrong, every trial fails.
  .harnessCli(
    ['node', './src/harness/run-trial.js'],
    { integrationLevel: 'cli_events' }
  )

  .baseline('control', { model: 'gpt-4o', temperature: 0.0 })
  .addVariant('treatment', { model: 'gpt-4o', temperature: 0.7 })

  // Metrics — each declares its source explicitly
  .metric(Metric.DURATION_MS)                                          // runner tracks
  .metric(Metric.TOKENS_IN)                                            // from events
  .metric(Metric.TOKENS_OUT)                                           // from events
  .metric(Metric.fromOutput('success', '/outcome', {                   // from trial_output
    primary: true, weight: 1.0, direction: 'maximize',
  }))
  .metric(Metric.fromOutput('cost_usd', '/metrics/cost_usd', {
    direction: 'minimize',
  }))

  // Artifacts — collect workspace output and track changes
  .artifacts({ collect: ['**/*.py', 'output/**'], diff: true })
  .metric(Metric.FILES_MODIFIED)                                          // from artifacts
  .metric(Metric.DIFF_LINES)                                              // from artifacts

  .networkMode('allowlist_enforced', ['api.openai.com']);

// 2. Write config to disk
mkdirSync('.lab', { recursive: true });
writeFileSync('.lab/experiment.yaml', builder.toYaml());

// 3. Validate and run
const client = new LabClient();

const summary = await client.describe({ experiment: '.lab/experiment.yaml' });
console.log(`Planned: ${summary.summary.total_trials} trials`);

const run = await client.run({ experiment: '.lab/experiment.yaml' });
console.log(`Done: ${run.run.run_id}`);
```

## ExperimentBuilder

Fluent API for building `ExperimentSpec` objects. All required fields must be explicitly set — `build()` validates completeness and throws listing any missing fields.

```ts
const builder = ExperimentBuilder.create('id', 'Name')
```

### Required methods

These must be called before `build()` or `toYaml()`:

| Method | What it sets |
|---|---|
| `.datasetJsonl(path, opts)` | Dataset source. `opts` requires `suiteId`, `splitId`, `limit`. |
| `.harnessCli(command, opts)` | Harness command array. `opts` requires `integrationLevel`. |

### Optional methods

| Method | What it sets | Default |
|---|---|---|
| `.sanitizationProfile(value)` | Sanitization profile name. | `'hermetic_functional_v2'` |
| `.replications(n)` | How many times each (task, variant) pair runs. | `1` |
| `.randomSeed(n)` | Seed for trial ordering reproducibility. | `1` |
| `.description(text)` | Experiment description. | |
| `.owner(name)` | Experiment owner. | |
| `.tags(list)` | Tag array. | |
| `.baseline(id, bindings)` | Baseline variant with parameter bindings. | `{ variant_id: 'base', bindings: {} }` |
| `.addVariant(id, bindings)` | Additional variant. Call multiple times for multiple variants. | |
| `.maxConcurrency(n)` | Parallel trial limit. | `1` |
| `.metric(def)` | Add a metric definition. See Metrics below. | |
| `.guardrail(def)` | Add a budget guardrail. See Guardrails below. | |
| `.artifacts(opts)` | Configure workspace artifact collection. See Artifacts below. | |
| `.networkMode(mode, hosts?)` | `'none'`, `'full'`, or `'allowlist_enforced'` with allowed hosts. | `'none'` |
| `.sandboxImage(image)` | Docker image name. Sets sandbox mode to `container`. | |
| `.localSandbox()` | Run without container isolation. | `local` |

### Terminal methods

| Method | What it returns |
|---|---|
| `.build()` | Deep-copied `ExperimentSpec`. Throws if required fields are missing. |
| `.toYaml()` | YAML string of the spec. Validates completeness first. |

All setters return `this` for chaining.

### Metrics

Each metric declares exactly where its value comes from. No magic — if a metric isn't declared, it isn't tracked (except runner auto-metrics which are always collected).

**Four sources:**

| Source | What it means | Example |
|---|---|---|
| `runner` | Runner measures this automatically | Wall-clock duration, exit code |
| `events` | Runner aggregates from harness hook events | Token counts, step counts |
| `output` | Runner extracts from `trial_output.json` | Accuracy, cost, any field your harness writes |
| `artifacts` | Runner computes from workspace changes | Files created/modified, diff size |

**Predefined constants** (use directly with `.metric()`):

| Constant | Source | What it tracks |
|---|---|---|
| `Metric.DURATION_MS` | runner | Trial wall-clock time |
| `Metric.EXIT_CODE` | runner | Harness process exit code |
| `Metric.TOKENS_IN` | events | Sum of input tokens from `model_call_end` events |
| `Metric.TOKENS_OUT` | events | Sum of output tokens from `model_call_end` events |
| `Metric.STEP_COUNT` | events | Count of `agent_step_start` events |
| `Metric.TURN_COUNT` | events | Count of `model_call_end` events |
| `Metric.TOOL_CALL_COUNT` | events | Count of `tool_call_end` events |
| `Metric.FILES_CREATED` | artifacts | Number of new files in workspace |
| `Metric.FILES_MODIFIED` | artifacts | Number of modified files in workspace |
| `Metric.DIFF_BYTES` | artifacts | Total bytes of workspace diff |
| `Metric.DIFF_LINES` | artifacts | Total lines of workspace diff |

**Factories** for custom metrics:

```ts
// Extract a value from trial_output.json by JSON pointer
Metric.fromOutput('accuracy', '/metrics/accuracy', {
  primary: true,       // highlighted in analysis summaries
  weight: 1.0,         // contributes to composite score (0 = observe only)
  direction: 'maximize',
})

// Aggregate a field across hook events in a trial
Metric.fromEvents('avg_model_latency', {
  eventType: 'model_call_end',
  eventField: 'timing.duration_ms',
  aggregate: 'mean',    // sum | count | max | min | mean | last
  direction: 'minimize',
})

// Measure workspace artifacts (requires .artifacts() config)
Metric.fromArtifacts('py_patch_size', {
  measure: 'diff_bytes',  // file_count | diff_bytes | diff_lines | total_bytes
  glob: '**/*.py',         // optional — scope to matching files
  direction: 'minimize',
})
```

### Artifacts

Configure artifact collection to capture what your harness writes to the workspace. The runner snapshots the workspace before the trial, then collects matching files and computes diffs after the trial completes.

```ts
builder
  .artifacts({
    collect: ['**/*.py', 'output/**'],  // glob patterns for files to preserve
    diff: true,                          // compute workspace diff (pre vs post)
    baseDir: 'workspace/src',            // optional: scope collection to subdirectory
  })
  .metric(Metric.FILES_MODIFIED)
  .metric(Metric.DIFF_LINES)
  .metric(Metric.fromArtifacts('py_changes', {
    measure: 'diff_bytes',
    glob: '**/*.py',
    direction: 'minimize',
  }))
```

Collected artifacts and diffs are stored in each trial's directory under the run artifacts.

### Guardrails

Budget guardrails set upper bounds on metrics. When a trial exceeds a guardrail limit, the runner fails it. Guardrails reference metrics by `metric_id` — the metric must also be declared via `.metric()`.

```ts
builder
  .metric(Metric.TOKENS_IN)
  .metric(Metric.TOKENS_OUT)
  .metric(Metric.DURATION_MS)
  .metric(Metric.TOOL_CALL_COUNT)
  .guardrail(Metric.maxTokensIn(50_000))     // fail trial if input tokens exceed 50k
  .guardrail(Metric.maxTokensOut(10_000))     // fail trial if output tokens exceed 10k
  .guardrail(Metric.maxDuration(300_000))     // fail trial if wall-clock exceeds 5 minutes
  .guardrail(Metric.maxToolCalls(100))        // fail trial if tool calls exceed 100
```

**Predefined guardrail factories:**

| Factory | metric_id | What it limits |
|---|---|---|
| `Metric.maxTokensIn(n)` | `tokens_in` | Total input tokens per trial |
| `Metric.maxTokensOut(n)` | `tokens_out` | Total output tokens per trial |
| `Metric.maxDuration(ms)` | `duration_ms` | Trial wall-clock time in ms |
| `Metric.maxToolCalls(n)` | `tool_call_count` | Number of tool invocations |
| `Metric.maxTurns(n)` | `turn_count` | Number of model turns |
| `Metric.maxCost(n)` | `cost_usd` | Cost (requires `cost_usd` output metric) |

Custom guardrails work with any metric:

```ts
builder
  .metric(Metric.fromOutput('cost_usd', '/metrics/cost_usd'))
  .guardrail({ metric_id: 'cost_usd', max: 5.0 })
```

Guardrails are lazy — no `guardrails:` section appears in the YAML unless you add at least one. Calling `.guardrail()` with the same `metric_id` replaces the previous entry.

## LabClient

Spawns the Rust `lab` binary and parses structured JSON responses.

```ts
const client = new LabClient({
  runnerBin: '/path/to/lab',   // or set AGENTLAB_RUNNER_BIN env var
  cwd: '/project/root',
  env: { OPENAI_API_KEY: '...' },
});
```

### Runner discovery

Resolves the binary in order:

1. `runnerBin` constructor option
2. `AGENTLAB_RUNNER_BIN` environment variable
3. `lab` (assumes on `PATH`)

### Commands

| Method | Returns | Description |
|---|---|---|
| `describe(args)` | `DescribeResponse` | Dry-run: planned trials and resolved config |
| `run(args)` | `RunResponse` | Execute trials with configured network and sandbox mode |
| `runDev(args)` | `RunResponse` | Dev run: full network, optional `setup` command |
| `replay(args)` | `ReplayResponse` | Re-execute a trial from run artifacts |
| `fork(args)` | `ForkResponse` | Fork a trial at a checkpoint with binding overrides |
| `pause(args)` | `PauseResponse` | Cooperative pause via checkpoint+stop handshake |
| `resume(args)` | `ResumeResponse` | Resume a paused trial |
| `publish(args)` | `PublishResponse` | Create debug bundle from a run |
| `validateKnobs(args)` | `ValidateResponse` | Validate parameter overrides against manifest |
| `validateHooks(args)` | `ValidateResponse` | Validate event stream against harness manifest |
| `validateSchema(args)` | `ValidateResponse` | Validate JSON file against schema |
| `readAnalysis(args)` | `ReadAnalysisResponse` | Read analysis summary and comparisons from a run directory |

All commands accept per-call `cwd` and `env` overrides.

### Analysis access

After a run completes, `readAnalysis()` reads the analysis files from the run directory. This is pure file I/O — no CLI spawn.

```ts
const result = await client.readAnalysis({ runDir: '.lab/runs/run_20260211_120000' });

// Per-variant summary
for (const [id, variant] of Object.entries(result.summary.variants)) {
  console.log(`${id}: ${variant.success_rate} success, ${variant.event_counts.model_call_end} LLM calls`);
}

// Pairwise comparisons
for (const cmp of result.comparisons.comparisons) {
  console.log(`${cmp.baseline} vs ${cmp.variant}: ${cmp.baseline_success_rate} → ${cmp.variant_success_rate}`);
}
```

### Control lifecycle

```ts
const client = new LabClient();
const runDir = '.lab/runs/run_20260211_120000';

// Pause at next safe boundary
const paused = await client.pause({
  runDir,
  trialId: 'trial_001',
  label: 'before_tool_call',
  timeoutSeconds: 90,
});

// Fork from checkpoint with modified bindings
const forked = await client.fork({
  runDir,
  fromTrial: paused.pause.trial_id,
  at: 'checkpoint:before_tool_call',
  set: { model: 'gpt-4.1-mini', temperature: 0.2 },
  strict: true,
});

// Resume the original trial
const resumed = await client.resume({
  runDir,
  trialId: paused.pause.trial_id,
  label: 'before_tool_call',
  set: { max_steps: 50 },
});

// Replay for validation
const replayed = await client.replay({
  runDir,
  trialId: paused.pause.trial_id,
  strict: true,
});
```

### Error handling

All runner failures throw `LabRunnerError`:

```ts
import { LabRunnerError } from '@agentlab/sdk';

try {
  await client.run({ experiment: 'experiment.yaml' });
} catch (err) {
  if (err instanceof LabRunnerError) {
    err.code;      // 'bad_config', 'spawn_failed', 'invalid_json', etc.
    err.message;   // Human-readable description
    err.command;   // Full command array that was spawned
    err.stderr;    // Runner stderr output
    err.exitCode;  // Process exit code (if available)
    err.details;   // Structured error details (if available)
  }
}
```

## Trial Output Types

Type declarations mirroring `trial_output_v1.jsonschema`. Use these to type-check what your harness writes.

```ts
import type { TrialOutput, TrialIds, TrialOutcome } from '@agentlab/sdk';

const output: TrialOutput = {
  schema_version: 'trial_output_v1',
  ids: { run_id: 'run_001', trial_id: 'trial_001', variant_id: 'baseline', task_id: 'task_001', repl_idx: 0 },
  outcome: 'success',
  answer: 'The fix is ...',
  metrics: { accuracy: 0.95, cost_usd: 0.12 },
  objective: [{ name: 'resolved', value: 1.0, direction: 'maximize' }],
  artifacts: [{ path: '/out/patch.diff', logical_name: 'solution_diff' }],
  checkpoints: [{ path: '/state/cp1.json', logical_name: 'after_analysis', step: 3 }],
};
```

| Type | What it represents |
|---|---|
| `TrialOutput` | Top-level trial output object |
| `TrialIds` | `{ run_id, trial_id, variant_id, task_id, repl_idx }` — shared with event types |
| `TrialOutcome` | `'success' \| 'failure' \| 'missing' \| 'error'` |
| `TrialError` | `{ error_type?, message?, stack? }` |
| `ArtifactDecl` | `{ path, logical_name?, mime_type? }` |
| `CheckpointDecl` | `{ path, logical_name?, step?, epoch? }` |
| `ObjectiveDef` | `{ name, value, direction? }` |

## Event Stream Types

Typed discriminated union for the `harness_events.jsonl` event stream (at `cli_events` integration level and above). Enables structural diffing of variant behavior — same task, two variants, compare step-by-step what each agent did.

```ts
import type { HookEvent, ModelCallEndEvent } from '@agentlab/sdk';

function analyzeEvents(events: HookEvent[]) {
  for (const e of events) {
    switch (e.event_type) {
      case 'agent_step_start':
        console.log(`Step ${e.step_index} started`);
        break;
      case 'model_call_end':
        console.log(`Model call: ${e.usage?.tokens_in} in, ${e.usage?.tokens_out} out`);
        break;
      case 'tool_call_end':
        console.log(`Tool: ${e.tool.name} — ${e.outcome.status}`);
        break;
      case 'error':
        console.log(`Error: ${e.message}`);
        break;
    }
  }
}
```

**6 event types** (discriminated on `event_type`):

| Type | Key fields | Purpose |
|---|---|---|
| `AgentStepStartEvent` | `step_index` | Opens an agent step |
| `AgentStepEndEvent` | `step_index`, `budgets?` | Closes a step with running budget totals |
| `ModelCallEndEvent` | `call_id`, `outcome`, `model?`, `usage?`, `timing?` | Records an LLM call |
| `ToolCallEndEvent` | `call_id`, `tool`, `outcome`, `timing?` | Records a tool invocation |
| `ControlAckEvent` | `step_index`, `control_version`, `action_observed` | Acknowledges control plane signal |
| `ErrorEvent` | `message`, `error_type?`, `stack?` | General error |

**Shared sub-types:** `CallOutcome`, `StepBudgets`, `ModelIdentity`, `CallTiming`, `RedactionInfo`, `HookEventBase`

## Exports

```ts
// Classes
export { LabClient, LabRunnerError } from '@agentlab/sdk';
export { ExperimentBuilder, Metric } from '@agentlab/sdk';

// Experiment builder types
export type {
  ExperimentSpec, MetricDef, MetricSource, MetricAggregate,
  ArtifactMeasure, GuardrailDef, Bindings,
  DatasetJsonlOptions, HarnessCliOptions,
} from '@agentlab/sdk';

// Client types
export type {
  LabClientOptions, LabErrorEnvelope, LabErrorPayload,
  DescribeArgs, DescribeResponse, ExperimentSummary,
  RunArgs, RunDevArgs, RunResponse,
  ReplayArgs, ReplayResponse,
  ForkArgs, ForkResponse,
  PauseArgs, PauseResponse,
  ResumeArgs, ResumeResponse,
  PublishArgs, PublishResponse,
  KnobsValidateArgs, HooksValidateArgs, SchemaValidateArgs,
  ValidateResponse,
  ReadAnalysisArgs, ReadAnalysisResponse,
  AnalysisSummary, AnalysisComparisons, ComparisonEntry,
  VariantSummary, EventCounts,
} from '@agentlab/sdk';

// Trial output types
export type {
  TrialOutput, TrialIds, TrialOutcome, TrialError,
  ArtifactDecl, CheckpointDecl, ObjectiveDef,
} from '@agentlab/sdk';

// Event stream types
export type {
  HookEvent, HookEventBase,
  AgentStepStartEvent, AgentStepEndEvent,
  ModelCallEndEvent, ToolCallEndEvent,
  ControlAckEvent, ErrorEvent,
  CallOutcome, StepBudgets, ModelIdentity,
  CallTiming, RedactionInfo,
} from '@agentlab/sdk';
```
