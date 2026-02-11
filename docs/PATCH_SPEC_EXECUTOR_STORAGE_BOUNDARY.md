# Patch Spec: Executor/Storage Boundary + Optional Local Materialization

## Goal

Allow runs to execute locally or remotely without changing Runner Core semantics.

- Runner Core must not assume local filesystem persistence.
- Event, artifact, output, and control touchpoints must be first-class across backends.
- Local disk writes become policy-driven (`none | metadata_only | outputs_only | full`).

---

## Problem

Current Rust runner mixes three responsibilities in one loop:

1. Scheduling/analysis logic (core)
2. Trial execution transport (local process / Docker CLI)
3. Persistence layout (`.lab/runs/...` paths and file contracts)

This prevents clean remote execution, because the core directly depends on host paths and bind mounts.

---

## Non-Goals

- No requirement to remove current local Docker behavior.
- No requirement to implement cloud orchestration in this patch.
- No requirement to change trial input/output schemas immediately.

---

## Proposed Architecture

### 1. Runner Core uses an executor interface

Introduce a transport-agnostic interface:

```ts
interface TrialExecutor {
  startTrial(req: StartTrialRequest): Promise<TrialHandle>;
  getTrialStatus(trialId: string): Promise<TrialStatus>;
  streamTrialEvents(trialId: string, fromSeq?: number): AsyncIterable<HookEvent>;
  sendControl(trialId: string, action: ControlAction): Promise<ControlReceipt>;
  fetchTrialOutput(trialId: string): Promise<TrialOutputRef>;
  listTrialArtifacts(trialId: string): Promise<ArtifactRef[]>;
  getArtifact(ref: ArtifactRef): Promise<ReadableStream<Uint8Array>>;
  cancelTrial(trialId: string): Promise<void>;
}
```

Runner Core only depends on this interface and analysis logic.

### 2. Add persistence/materialization policy

Introduce run-time policy:

```ts
type MaterializationMode = 'none' | 'metadata_only' | 'outputs_only' | 'full';
```

Semantics:

- `none`: keep refs only; no automatic local files.
- `metadata_only`: persist run/trial metadata and summaries, not full artifacts.
- `outputs_only`: persist trial outputs + metadata, artifacts by ref.
- `full`: current behavior (outputs, events, artifacts, state snapshots).

### 3. Separate logical refs from physical files

Define immutable refs for all produced evidence:

```ts
interface BlobRef {
  uri: string;            // e.g. s3://..., file://..., cas://sha256:...
  digest: string;         // sha256:...
  size_bytes: number;
  media_type?: string;
}

interface TrialOutputRef extends BlobRef {}
interface EventLogRef extends BlobRef {}
interface ArtifactRef extends BlobRef {
  artifact_type?: 'workspace_diff' | 'log' | 'trace' | 'custom';
}
```

Runner analysis consumes refs, not path assumptions.

### 4. Backend adapters

- `LocalDockerExecutor`: current container behavior; maps refs to local files.
- `LocalProcessExecutor`: no container.
- `RemoteJobExecutor` (new interface target): submit/poll/stream against remote control plane.

### 5. Compatibility with new SDK canonical types

Use existing SDK canonical types as the source of truth for the SDK surface:

- `sdk/src/hook-events.ts` (`HookEvent*`)
- `sdk/src/trial-output.ts` (`TrialOutput*`)
- `sdk/src/experiment-builder.ts` (`GuardrailDef`)

Do not introduce duplicate event/output type families in this patch.

Type/schema constraints that must hold during implementation:

1. `ControlAckEvent.control_version` must stay schema-compatible (`sha256:<64-hex>` string), not numeric.
2. `ControlAckEvent.action_observed` / `action_taken` should align to canonical action enum (`continue|stop|checkpoint`).
3. `TrialOutput.objective` must match current `trial_output_v1` schema shape (single object). If multi-objective is desired, that is a separate schema version change.
4. Guardrails remain experiment-design metadata in this patch (no required runner enforcement changes unless explicitly enabled in follow-up).

---

## Data Flow Contract

Per trial, the contract is:

1. Runner Core calls `startTrial(StartTrialRequest)`.
2. Executor returns `trial_id` + handles.
3. Runner Core optionally streams events (`streamTrialEvents`) for live metrics/control checks.
4. On terminal status, Runner Core obtains `TrialOutputRef`.
5. Runner Core lists artifact refs and fetches only what policy needs.
6. Runner Core writes analysis tables and final run summary via a persistence sink.

Control-plane behavior:

- `sendControl()` is transport-level command.
- `control_ack` remains evidence-level requirement (events or equivalent receipt).

---

## API / CLI Changes

### Rust CLI (`lab run`)

Add flags:

- `--executor local_docker|local_process|remote`
- `--materialize none|metadata_only|outputs_only|full`
- `--remote-endpoint <url>` (required when `executor=remote`)
- `--remote-token-env <ENV_NAME>` (optional)

Defaults:

- `executor=local_docker` when container mode is configured.
- `materialize=full` for local executors (backward compatible).
- `materialize=metadata_only` for remote executor unless overridden.

### TypeScript SDK

Extend `RunArgs`:

```ts
interface RunArgs {
  experiment: string;
  overrides?: string;
  executor?: 'local_docker' | 'local_process' | 'remote';
  materialize?: 'none' | 'metadata_only' | 'outputs_only' | 'full';
  remoteEndpoint?: string;
  remoteTokenEnv?: string;
}
```

---

## Storage/Persistence Abstraction

Introduce a sink interface used by Runner Core:

```ts
interface RunPersistenceSink {
  writeRunMetadata(run: RunMetadata): Promise<void>;
  writeTrialRecord(record: TrialRecord): Promise<void>;
  writeAnalysisTable(name: string, rows: AsyncIterable<Record<string, unknown>>): Promise<void>;
  maybeMaterialize(ref: BlobRef, class: 'output'|'events'|'artifact', mode: MaterializationMode): Promise<void>;
}
```

Implementations:

- `LocalFilesystemSink` -> `.lab/runs/<run_id>/...`
- `RemoteCatalogSink` -> metadata DB / object refs
- `HybridSink` -> remote primary + local cache

---

## File-Level Patch Plan

### Rust runner

- `rust/crates/lab-runner/src/lib.rs`
  - Extract run loop transport calls behind `TrialExecutor` trait.
  - Replace direct path-based output/event/artifact reads with ref-based fetch APIs.
  - Keep analysis pipeline unchanged at semantic level.

- `rust/crates/lab-runner/src/executor.rs` (new)
  - Define `TrialExecutor`, request/response structs, status enums.
  - Implement `LocalDockerExecutor` using existing `docker run` logic.
  - Implement `LocalProcessExecutor` from existing non-container logic.

- `rust/crates/lab-runner/src/persistence.rs` (new)
  - Define `RunPersistenceSink` and materialization policy.
  - Implement `LocalFilesystemSink` (current layout).

- `rust/crates/lab-runner/src/remote_executor.rs` (new, scaffold)
  - Stub HTTP transport for `start/status/events/output/artifacts/control`.
  - Return typed errors for unimplemented endpoints.

### Rust CLI

- `rust/crates/lab-cli/src/main.rs`
  - Parse `--executor`, `--materialize`, `--remote-endpoint`, `--remote-token-env`.
  - Pass through to lab-runner run options.

### TypeScript SDK

- `sdk/src/types.ts`
  - Add executor/materialization fields to `RunArgs`.

- `sdk/src/client.ts`
  - Map new args to CLI flags.

- `sdk/src/index.ts`
  - Re-export updated run arg types.

### Docs

- `README.md`
  - Add executor + materialization matrix.

- `docs/implementation_spec.md`
  - Add note that local file layout is one sink implementation, not required by core contract.

---

## Backward Compatibility

- Default local behavior remains the same (`materialize=full`).
- Existing `.lab/runs/<run_id>/...` layout preserved for local mode.
- Existing harness CLI contract (`AGENTLAB_TRIAL_INPUT`, `AGENTLAB_TRIAL_OUTPUT`) remains valid.

---

## Acceptance Criteria

1. Running with `--executor local_docker --materialize full` produces current artifact layout.
2. Running with `--executor local_docker --materialize metadata_only` completes successfully while skipping heavy artifact copies.
3. Runner Core compiles/tests without importing Docker-specific code paths directly.
4. Analysis tables are generated from trial records + fetched outputs regardless of executor backend.
5. Remote executor mode can execute a mocked run where outputs/events/artifacts are fetched via refs only.
6. `control_ack` evidence is still validated for `cli_events` and above.

---

## Test Plan

1. Unit tests: executor trait adapters and ref serialization.
2. Unit tests: materialization policy behavior (`none`, `metadata_only`, `outputs_only`, `full`).
3. Integration test: local docker run parity with baseline outputs.
4. Integration test: mock remote executor returns refs; analysis still produced.
5. Regression test: existing SDK `client.run()` call path unchanged when new args are omitted.

---

## Open Questions

1. Should `materialize=none` still persist a minimal local `run_manifest.json` for recovery?
2. For remote events, do we require durable ordered logs or allow best-effort streams + checkpointed pagination?
3. Should artifact refs be mandatory SHA256-addressed CAS URIs (`cas://sha256:...`) in v1?
