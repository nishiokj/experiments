# Patch Spec: Defaults + Run Mode Collapse

## Goal

Reduce API friction without adding surface area. Two changes: restore strict-safe defaults so `build()` requires only dataset + harness, and collapse three run modes into two.

---

## 1. Restore ExperimentBuilder Defaults

### Problem

`build()` currently requires 9 fields. Six of those have exactly one obvious default. Users must call `.sanitizationProfile('hermetic_functional_v2')` and `.randomSeed(42)` every time, even though there's no real choice being made.

### Change

Restore defaults for fields that have a single strict-safe value. `build()` validation drops from 9 required fields to 4.

| Field | Current default | New default | Rationale |
|---|---|---|---|
| `sanitization_profile` | `''` (sentinel) | `'hermetic_functional_v2'` | Only profile that exists |
| `replications` | `0` (sentinel) | `1` | Run once is the obvious starting point |
| `random_seed` | `0` (sentinel) | `1` | Reproducible by default; change when you need different ordering |
| `dataset.path` | `''` (sentinel) | `''` (sentinel) | **No default** — only the user knows where their data is |
| `dataset.suite_id` | `''` (sentinel) | `''` (sentinel) | **No default** — user-specific |
| `dataset.split_id` | `''` (sentinel) | `''` (sentinel) | **No default** — user-specific |
| `dataset.limit` | `0` (sentinel) | `0` (sentinel) | **No default** — user must choose |
| `harness.command` | `[]` (sentinel) | `[]` (sentinel) | **No default** — only the user knows what to run |
| `harness.integration_level` | `''` (sentinel) | `''` (sentinel) | **No default** — user must choose |

### Minimum viable builder after change

```ts
ExperimentBuilder.create('my-exp', 'My Experiment')
  .datasetJsonl('./data/tasks.jsonl', { suiteId: 'suite', splitId: 'dev', limit: 50 })
  .harnessCli(['node', './harness.js'], { integrationLevel: 'cli_basic' })
  .build() // valid spec
```

### build() validation after change

Throws listing missing fields only for:

```
ExperimentBuilder: required fields not set:
  - dataset path (call .datasetJsonl())
  - dataset suite_id (call .datasetJsonl() with suiteId)
  - dataset split_id (call .datasetJsonl() with splitId)
  - dataset limit (call .datasetJsonl() with limit > 0)
  - harness command (call .harnessCli())
  - harness integration_level (call .harnessCli() with integrationLevel)
```

`.sanitizationProfile()`, `.replications()`, `.randomSeed()` become optional overrides. All three setters remain — they just aren't required anymore.

### Files

- `sdk/src/experiment-builder.ts` — change constructor defaults, remove three checks from `build()`
- `sdk/test/experiment-builder.test.ts` — update default assertions, update validation tests

---

## 2. Collapse Run Modes

### Problem

Three methods do two things:

| Method | Network | Container | Purpose |
|---|---|---|---|
| `run()` | As configured | `--container` flag | General |
| `runDev()` | Forced `full` | Forced on | Iteration |
| `runExperiment()` | Must be `none` | Forced on | Strict |

`run()` and `runExperiment()` overlap — `runExperiment()` is just `run()` with validation. Users don't know which to call. The naming suggests `runExperiment` is the "real" one, but `run` is what most people reach for first.

### Change

- **`run()`** becomes strict by default: validates network mode is `none`, runs in container mode. This is what was `runExperiment()`.
- **`runDev()`** stays as-is: forces full network, forced container, optional `--setup` command.
- **`runExperiment()`** is removed. Callers migrate to `run()`.

The old `run()` behavior (run with whatever config says, optional `--container` flag) is gone. If you need custom network + optional container, use `runDev()` or configure and call `run()`.

Wait — that breaks users who were calling `run()` for local development without containers. Let me reconsider.

**Revised change:**

- **`run(args)`** — executes trials with the experiment's configured network and sandbox mode. No overrides, no flags. What the config says is what happens. This is the current `run()` minus the `container` flag.
- **`runDev(args)`** — development mode. Forces network `full`, forces container, optional `--setup`. Same as today.
- **`runExperiment()`** — **removed**. Its validation (network must be `none`) moves to `run()` as a warning, not a hard error. If you configured `networkMode('none')` (the default), `run()` is already strict. If you configured `allowlist_enforced`, that's your choice.

This means: the experiment config is the source of truth. `run()` runs what you configured. `runDev()` overrides for iteration. No third mode.

### Migration

| Before | After |
|---|---|
| `client.runExperiment(args)` | `client.run(args)` |
| `client.run(args)` | `client.run(args)` (unchanged) |
| `client.run({ ..., container: true })` | Configure `.sandboxImage()` in builder, then `client.run(args)` |
| `client.runDev(args)` | `client.runDev(args)` (unchanged) |

### RunArgs change

```ts
// Before
interface RunArgs {
  experiment: string;
  overrides?: string;
  container?: boolean;  // removed
}

// After
interface RunArgs {
  experiment: string;
  overrides?: string;
}
```

The `container` flag was a runtime override for sandbox mode. That's config — it belongs in the experiment spec via `.sandboxImage()` or `.localSandbox()`, not as a per-call flag.

### Files

- `sdk/src/client.ts` — remove `runExperiment()`, remove `container` from `RunArgs`, update `run()` CLI args
- `sdk/src/types.ts` — remove `RunExperimentArgs`, remove `container` from `RunArgs`
- `sdk/src/index.ts` — remove `RunExperimentArgs` export
- `sdk/test/client.test.ts` — remove `runExperiment` tests, remove `container` flag tests from `run`
- `sdk/README.md` — update commands table, remove `runExperiment` references
- `README.md` — update run modes section

### CLI impact

The Rust CLI still has `run`, `run-dev`, and `run-experiment` commands. The SDK change doesn't require CLI changes — `client.run()` just calls `lab run` instead of `lab run-experiment`. The CLI commands can be consolidated separately.

---

## Execution Order

1. Restore defaults (no dependencies)
2. Collapse run modes (no dependencies on 1)
3. Update tests for both
4. Update both READMEs

## Verification

1. `npm run build` — compiles
2. `npm test` — all tests pass
3. `ExperimentBuilder.create('e', 'n').datasetJsonl(...).harnessCli(...).build()` succeeds without sanitizationProfile/replications/randomSeed
4. `client.run()` works, `client.runExperiment` no longer exists
