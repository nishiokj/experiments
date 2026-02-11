# @agentlab/sdk

TypeScript SDK for programmatic experiment construction and execution via the AgentLab Rust runner. Define experiments with a fluent builder API, execute them through a typed client, and get structured JSON results — no YAML hand-editing required.

## Install

```bash
npm install @agentlab/sdk
```

For local development:

```bash
cd sdk
npm install
npm run build
npm test
```

## Quick Start

```ts
import { ExperimentBuilder, LabClient } from '@agentlab/sdk';
import { writeFileSync, mkdirSync } from 'node:fs';

// 1. Build an experiment spec
const builder = ExperimentBuilder.create('prompt_ab', 'Prompt A/B Test')
  .description('Compare prompt v1 vs v2 on coding tasks')
  .datasetJsonl('tasks.jsonl', { suiteId: 'coding', splitId: 'dev', limit: 50 })
  .harnessCli(['node', './harness.js', 'run'], { integrationLevel: 'cli_events' })
  .sanitizationProfile('hermetic_functional_v2')
  .replications(3)
  .randomSeed(1337)
  .baseline('control', { prompt: 'prompt:v1' })
  .addVariant('treatment', { prompt: 'prompt:v2' })
  .primaryMetrics(['success', 'accuracy'])
  .secondaryMetrics(['latency_ms', 'cost_usd'])
  .networkMode('allowlist_enforced', ['api.openai.com']);

// 2. Write the YAML spec to disk
mkdirSync('.lab', { recursive: true });
writeFileSync('.lab/experiment.yaml', builder.toYaml());

// 3. Run it
const client = new LabClient();
const summary = await client.describe({ experiment: '.lab/experiment.yaml' });
console.log(`${summary.summary.total_trials} trials planned`);

const run = await client.runExperiment({ experiment: '.lab/experiment.yaml' });
console.log(`Run complete: ${run.run.run_id}`);
```

## ExperimentBuilder

Fluent API for constructing `ExperimentSpec` objects. Requires explicit configuration — `build()` validates completeness and throws listing any missing fields.

```ts
const builder = ExperimentBuilder.create('id', 'Name')
```

| Method | Required | Description |
|---|---|---|
| `.datasetJsonl(path, opts)` | **yes** | Dataset source (`suiteId`, `splitId`, `limit` are required) |
| `.harnessCli(command, opts)` | **yes** | Harness command (`integrationLevel` is required) |
| `.sanitizationProfile(value)` | **yes** | Sanitization profile name |
| `.replications(n)` | **yes** | Number of replications per (task, variant) pair |
| `.randomSeed(n)` | **yes** | Random seed for reproducibility |
| `.description(text)` | | Experiment description |
| `.owner(name)` | | Experiment owner |
| `.tags(list)` | | Tag array |
| `.baseline(id, bindings)` | | Baseline variant with parameter bindings |
| `.addVariant(id, bindings)` | | Additional variant (call multiple times) |
| `.maxConcurrency(n)` | | Parallel trial limit |
| `.primaryMetrics(names)` | | Primary success metrics |
| `.secondaryMetrics(names)` | | Secondary metrics |
| `.networkMode(mode, hosts?)` | | `'none'`, `'full'`, or `'allowlist_enforced'` with allowed hosts |
| `.sandboxImage(image)` | | Docker container image |
| `.localSandbox()` | | Run without container isolation |
| `.build()` | | Returns a deep-copied `ExperimentSpec` (validates completeness) |
| `.toYaml()` | | Serializes the spec to YAML (validates completeness) |

All setters return `this` for chaining.

## LabClient

Typed client that spawns the Rust `lab` binary and parses structured JSON responses.

```ts
const client = new LabClient({
  runnerBin: '/path/to/lab', // or set AGENTLAB_RUNNER_BIN
  cwd: '/project/root',
  env: { OPENAI_API_KEY: '...' },
});
```

### Runner Discovery

`LabClient` resolves the runner binary in order:

1. `runnerBin` constructor option
2. `AGENTLAB_RUNNER_BIN` environment variable
3. `lab` (assumes on `PATH`)

### Commands

| Method | Returns | Description |
|---|---|---|
| `client.describe(args)` | `DescribeResponse` | Experiment summary without execution |
| `client.run(args)` | `RunResponse` | Run experiment (optional `container` flag) |
| `client.runDev(args)` | `RunResponse` | Development run with optional `setup` command |
| `client.runExperiment(args)` | `RunResponse` | Strict run with network isolation |
| `client.replay(args)` | `ReplayResponse` | Replay a prior trial from run artifacts |
| `client.fork(args)` | `ForkResponse` | Fork a trial at selector (`checkpoint`, `step`, `event_seq`) |
| `client.pause(args)` | `PauseResponse` | Cooperative pause via checkpoint+stop handshake |
| `client.resume(args)` | `ResumeResponse` | Resume paused trial via checkpoint-based continuation |
| `client.publish(args)` | `PublishResponse` | Create debug bundle from a run |
| `client.validateKnobs(args)` | `ValidateResponse` | Validate parameter overrides against manifest |
| `client.validateHooks(args)` | `ValidateResponse` | Validate event stream against harness manifest |
| `client.validateSchema(args)` | `ValidateResponse` | Validate JSON file against a schema |

All commands accept per-call `cwd` and `env` overrides.

### Control lifecycle example

```ts
import { LabClient } from '@agentlab/sdk';

const client = new LabClient();
const runDir = '.lab/runs/run_20260211_120000';

// 1) Pause a trial at the next safe boundary (checkpoint + stop handshake)
const paused = await client.pause({
  runDir,
  trialId: 'trial_001',
  label: 'before_tool_call',
  timeoutSeconds: 90,
});

// 2) Fork from a checkpoint with binding overrides
const forked = await client.fork({
  runDir,
  fromTrial: paused.pause.trial_id,
  at: 'checkpoint:before_tool_call',
  set: { model: 'gpt-4.1-mini', temperature: 0.2 },
  strict: true,
});

// 3) Resume the paused trial (implemented as checkpoint-based continuation)
const resumed = await client.resume({
  runDir,
  trialId: paused.pause.trial_id,
  label: 'before_tool_call',
  set: { max_steps: 50 },
});

// 4) Replay a trial for validation / debugging
const replayed = await client.replay({
  runDir,
  trialId: paused.pause.trial_id,
  strict: true,
});

console.log(forked.fork.fork_id, resumed.resume.fork.fork_id, replayed.replay.replay_id);
```

### Error Handling

All runner failures throw `LabRunnerError` with structured context:

```ts
import { LabRunnerError } from '@agentlab/sdk';

try {
  await client.run({ experiment: 'experiment.yaml' });
} catch (err) {
  if (err instanceof LabRunnerError) {
    console.error(err.code);     // e.g. 'bad_config', 'spawn_failed', 'invalid_json'
    console.error(err.message);  // Human-readable message
    console.error(err.command);  // Full command array that was spawned
    console.error(err.stderr);   // Runner stderr output
    console.error(err.exitCode); // Process exit code (if available)
    console.error(err.details);  // Structured error details (if available)
  }
}
```

## Exports

```ts
// Classes
export { LabClient, LabRunnerError } from '@agentlab/sdk';
export { ExperimentBuilder } from '@agentlab/sdk';

// Types
export type {
  ExperimentSpec,
  DatasetJsonlOptions,
  HarnessCliOptions,
  LabClientOptions,
  DescribeArgs,
  DescribeResponse,
  ExperimentSummary,
  RunArgs,
  RunDevArgs,
  RunExperimentArgs,
  RunResponse,
  ReplayArgs,
  ReplayResponse,
  ForkArgs,
  ForkResponse,
  PauseArgs,
  PauseResponse,
  ResumeArgs,
  ResumeResponse,
  PublishArgs,
  PublishResponse,
  KnobsValidateArgs,
  HooksValidateArgs,
  SchemaValidateArgs,
  ValidateResponse,
  LabErrorEnvelope,
  LabErrorPayload,
} from '@agentlab/sdk';
```
