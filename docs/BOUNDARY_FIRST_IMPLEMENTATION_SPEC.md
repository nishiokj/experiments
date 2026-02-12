# Boundary-First Experiment + Benchmark Implementation Spec

Status: Draft v0.1  
Date: 2026-02-12

## 1) Objective

Define a strict, non-hand-wavy implementation model where:

- Harness is treated as an external API surface (not AgentLab-owned logic).
- Runner owns execution control and evidence capture.
- Benchmark adapters own benchmark semantics and scoring truth.
- Analysis layer owns aggregation and reporting.

This spec is intentionally boundary-first to prevent abstraction explosion.

## 2) Problem Statement

The current CLI harness contract allows harnesses to write `trial_output` directly, but this couples benchmark semantics and result-shaping to harness implementation details.

Required end state:

- Harness returns what it naturally returns.
- Runner captures observable evidence deterministically.
- Benchmark scoring derives truth from evidence and evaluator rules.
- Run-level outputs are reproducible and benchmark-agnostic.

## 3) Core Principles

1. Harness is BYO and API-like.  
2. Source of truth for benchmark verdicts is evaluator output, not harness self-report.  
3. Runner never embeds benchmark-specific scoring logic.  
4. Every task-level claim must be backed by stored evidence refs.  
5. Dependent-task benchmarks are first-class (stateful chains, not forced reset).  
6. Build and run are separate phases with explicit contracts.

## 4) Firm Boundaries (Owner + Contract)

| Boundary | Owner | Input | Output | Forbidden Coupling |
|---|---|---|---|---|
| `Build` | SDK | user config + input mapper | resolved experiment spec + task boundary dataset | no execution, no grading |
| `Run Orchestration` | Runner | resolved spec + task boundaries | run/trial directories + evidence records | no benchmark-specific scoring |
| `Harness Invocation` | Harness Adapter | harness request | harness response + raw emissions | no AgentLab schema ownership required |
| `Evidence Capture` | Runner | process/container/filesystem/events | canonical evidence records | no verdict inference |
| `Predict Mapping` | Benchmark Adapter | evidence records | prediction records | no run scheduling |
| `Score` | Benchmark Adapter/Evaluator | prediction records | score records | no execution side-effects |
| `Aggregate` | Analysis | score records + runner trial tables | summaries/comparisons/grades | no harness control |

## 5) Build vs Run Boundary

### Build Phase (SDK)

Build phase produces only static artifacts:

- `ExperimentSpec` (or YAML).
- Dataset rows compiled to task boundaries.
- Optional runner boundary manifest.

Build phase does not:

- launch harness,
- touch container/runtime state,
- compute benchmark verdicts.

### Run Phase (Runner)

Run phase consumes static artifacts and produces:

- per-trial execution artifacts,
- per-task evidence records,
- benchmark prediction/score artifacts (via adapter),
- run-level analysis and grades.

## 6) Policy Model

Policy resolution is deterministic and per task.

### 6.1 ExperimentTypePolicy

Controls experiment-level behavior:

- scheduling strategy,
- retry policy,
- comparison policy,
- materialization policy,
- default state policy.

### 6.2 BenchmarkTypePolicy

Controls benchmark semantics:

- independent vs dependent tasks,
- reset strategy,
- evaluator mode (`official`, `custom`),
- scoring lifecycle (`predict_then_score`, `integrated_score`),
- required evidence classes.

### 6.3 Effective Task Policy

Effective policy for task `t` is:

`effective = merge(global_defaults, ExperimentTypePolicy, BenchmarkTypePolicy, task_override)`

Merge must be logged in trial metadata for auditability.

## 7) Task Models

### 7.1 Independent Task Model

Each task runs from a reset baseline.

- Baseline snapshot for task.
- Harness invocation.
- Post snapshot.
- Diff is `baseline -> post`.
- Score is independent of other tasks.

### 7.2 Dependent Task Model (Stateful Chain)

Tasks share mutable state within a chain.

- Chain start creates `chain_root_snapshot`.
- For step `k`, runner captures:
  - `prev_snapshot` (state at `k-1`),
  - `post_snapshot` (state at `k`).
- Runner computes:
  - `incremental_diff = prev_snapshot -> post_snapshot`,
  - `cumulative_diff = chain_root_snapshot -> post_snapshot`.

Both diff types are preserved; adapter chooses which one to grade.

## 8) Evidence Source of Truth

Runner must collect evidence before any scoring:

- harness request and response payloads (or refs),
- stdout/stderr refs,
- hook events JSONL ref (if present),
- workspace snapshot refs,
- incremental and cumulative diff refs,
- patch refs (derived from diffs when needed),
- timing/resource usage,
- task/run identifiers.

Evidence records must be immutable and content-addressed.

## 9) Required Artifacts (Run Scope)

Runner-owned:

- `.lab/runs/<run_id>/trials/<trial_id>/...`
- `.lab/runs/<run_id>/evidence/evidence_records.jsonl`

Adapter-owned:

- `.lab/runs/<run_id>/benchmark/adapter_manifest.json`
- `.lab/runs/<run_id>/benchmark/predictions.jsonl`
- `.lab/runs/<run_id>/benchmark/scores.jsonl`
- `.lab/runs/<run_id>/benchmark/summary.json`

Analysis-owned:

- `.lab/runs/<run_id>/analysis/summary.json`
- `.lab/runs/<run_id>/analysis/comparisons.json`
- table outputs for joins and audit.

## 10) Contract Schemas to Standardize Next

1. `evidence_record_v1`  
2. `benchmark_prediction_record_v1`  
3. `benchmark_score_record_v1`  
4. `benchmark_summary_v1`  
5. `task_chain_state_v1` (for dependent-task chains)

## 11) Diff and Patch Collection Ownership

Diff collection is a runner concern.

Why:

- runner owns filesystem/container surfaces,
- runner can enforce deterministic snapshot timing,
- adapters should not depend on host/container internals.

Patch generation is derived from runner-captured diffs, not harness-author-provided strings by default.

Harness-provided patch can be accepted as supplemental evidence, never sole truth.

## 12) Scoring and Aggregation Ownership

Scoring is adapter/evaluator-owned.

- Official evaluator outputs are canonical where available.
- Custom evaluator must emit explicit evaluator identity/version.

Aggregation is analysis-owned.

- joins by `{run_id, trial_id, variant_id, task_id, repl_idx}`,
- computes pass rate, primary metrics, missingness, retry effects,
- emits comparisons and report-grade summaries.

## 13) Trade-offs Evaluation

### Option A: Harness writes canonical `trial_output` (current style)

Pros:

- simple harness contract,
- minimal runner logic.

Cons:

- harness implements AgentLab-specific schema behavior,
- benchmark truth can drift across harnesses,
- weak separation of concerns.

### Option B: Runner infers all results from raw events/diffs only

Pros:

- harness remains pure API.

Cons:

- impossible to infer benchmark-specific verdicts generally,
- overcouples runner to benchmark semantics.

### Option C (Recommended): Runner captures evidence, adapter scores, analysis aggregates

Pros:

- strict boundaries,
- benchmark truth remains benchmark-owned,
- supports independent and dependent tasks cleanly.

Cons:

- requires explicit adapter interfaces and schema work,
- slightly larger artifact surface.

### Option D: External async scorer service

Pros:

- scalable and decoupled scoring.

Cons:

- operational complexity,
- delayed feedback,
- queue/retry semantics required.

## 14) Recommended Decision Set

1. Adopt Option C as default architecture.  
2. Keep harness output file path support as compatibility mode, not canonical mode.  
3. Define `evidence_record_v1` as runner truth boundary.  
4. Make adapter `predict_then_score` default for benchmarks with official evaluators.  
5. Support dependent-task chains with dual diffs (incremental + cumulative).  
6. Make all run-level claims traceable to evidence refs.

## 15) Migration Plan from Current Runner

### Phase M1: Evidence-first capture

- Add evidence writer in runner.
- Preserve existing `trial_output` fallback behavior for compatibility.

### Phase M2: Adapter scoring integration

- Add adapter invocation after prediction capture.
- Emit `predictions.jsonl`, `scores.jsonl`, `benchmark/summary.json`.

### Phase M3: Canonical truth switch

- Analysis consumes adapter score records as primary verdict source.
- Harness-written `trial_output.metrics/objective` become optional hints unless adapter says otherwise.

### Phase M4: Contract hardening

- Add schema validation gates for evidence/prediction/score artifacts.
- Enforce fail-closed behavior for missing required evidence classes by benchmark policy.

## 16) Acceptance Criteria

1. Same harness binary can run under two benchmarks without code changes to "AgentLab output writing" paths.  
2. For official benchmarks, final verdicts match evaluator outputs exactly.  
3. For dependent chains, each task has both incremental and cumulative diff refs.  
4. Run report can trace every verdict back to evidence and evaluator metadata.  
5. Build phase artifacts are identical regardless of runtime executor choice.

## 17) Open Questions

1. Do we require patch normalization in runner or adapter?  
2. Which diff algorithm is canonical for cross-platform reproducibility?  
3. Should chain failure policy default to `stop_on_error` or `continue_with_flag`?  
4. Do we version ExperimentTypePolicy/BenchmarkTypePolicy independently of ExperimentSpec?
