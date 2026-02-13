import assert from 'node:assert/strict';
import test, { describe } from 'node:test';

import {
  assertTaskBoundaryV1,
  compileTaskBoundaries,
  createOutcomeBoundary,
  createRunnerBoundaryManifest,
  EVENT_OUTPUT_CONTRACT_V1,
  INVOCATION_ENV_CONTRACT_V1,
  mapOutcome,
  taskBoundariesToJsonl,
  WORKSPACE_CONTRACT_V1,
} from '../src/boundary-mappers.js';
import type {
  InputMapper,
  OutcomeMapper,
  TaskBoundaryV1,
} from '../src/boundary-mappers.js';
import type { HookEvent } from '../src/hook-events.js';
import type { TrialOutput } from '../src/trial-output.js';

function makeTaskBoundary(taskId: string): TaskBoundaryV1 {
  return {
    schema_version: 'task_boundary_v1',
    task: {
      id: taskId,
      prompt: `solve ${taskId}`,
    },
    workspace_files: [
      {
        path: 'README.md',
        content: `task ${taskId}`,
        encoding: 'utf8',
      },
    ],
    mount_references: [
      {
        dataset_pack_ref: `sha256:${'a'.repeat(64)}`,
        mount_path: '/workspace/dataset',
        read_only: true,
      },
    ],
    limits: {
      max_steps: 32,
      max_total_tokens: 12000,
      max_tool_calls: 20,
      trial_seconds: 300,
    },
  };
}

describe('Runner boundary contracts', () => {
  test('workspace and event contracts are fixed', () => {
    assert.equal(WORKSPACE_CONTRACT_V1.root, '/workspace');
    assert.equal(
      WORKSPACE_CONTRACT_V1.task_manifest_path,
      '/workspace/.agentlab/task-manifest.json',
    );
    assert.equal(WORKSPACE_CONTRACT_V1.artifacts_dir, '/workspace/.agentlab/artifacts');

    assert.equal(EVENT_OUTPUT_CONTRACT_V1.run_events_jsonl, '/state/harness_events.jsonl');
    assert.equal(EVENT_OUTPUT_CONTRACT_V1.result_summary, '/out/trial_output.json');
  });

  test('invocation env contract is fixed', () => {
    assert.equal(INVOCATION_ENV_CONTRACT_V1.trial_input, 'AGENTLAB_TRIAL_INPUT');
    assert.equal(INVOCATION_ENV_CONTRACT_V1.trial_output, 'AGENTLAB_TRIAL_OUTPUT');
    assert.equal(INVOCATION_ENV_CONTRACT_V1.control_path, 'AGENTLAB_CONTROL_PATH');
    assert.equal(INVOCATION_ENV_CONTRACT_V1.harness_root, 'AGENTLAB_HARNESS_ROOT');
  });

  test('manifest builder captures one-command invocation contract', () => {
    const manifest = createRunnerBoundaryManifest(['node', './harness.js', 'run']);
    assert.equal(manifest.schema_version, 'runner_boundary_manifest_v1');
    assert.deepEqual(manifest.invocation.command, ['node', './harness.js', 'run']);
    assert.equal(manifest.mount_semantics.read_only, true);
    assert.equal(manifest.mount_semantics.dataset_pack_ref_format, 'sha256:<hex64>');
  });

  test('manifest builder rejects empty command', () => {
    assert.throws(
      () => createRunnerBoundaryManifest([]),
      /invocation command must have at least one token/,
    );
  });
});

describe('InputMapper and task boundary', () => {
  test('compileTaskBoundaries maps source inputs to runner-consumable boundaries', () => {
    const mapper: InputMapper<{ id: string }> = {
      map(input) {
        return makeTaskBoundary(input.id);
      },
    };

    const boundaries = compileTaskBoundaries([{ id: 't1' }, { id: 't2' }], mapper);
    assert.equal(boundaries.length, 2);
    assert.equal(boundaries[0].task.id, 't1');
    assert.equal(boundaries[1].task.id, 't2');
  });

  test('assertTaskBoundaryV1 enforces abstraction boundary keys', () => {
    const invalid = {
      ...makeTaskBoundary('t1'),
      benchmark_kind: 'new_magic_type',
    };
    assert.throws(
      () => assertTaskBoundaryV1(invalid),
      /must compile into exactly: task \+ workspace_files \+ mount_references \+ limits/,
    );
  });

  test('mount references must be read-only dataset packs by hash', () => {
    const invalidRef = makeTaskBoundary('t1');
    invalidRef.mount_references[0].dataset_pack_ref = 'dataset-v1';
    assert.throws(
      () => assertTaskBoundaryV1(invalidRef),
      /dataset_pack_ref must match sha256:<hex64>/,
    );
  });

  test('workspace files must be relative to /workspace', () => {
    const invalidPath = makeTaskBoundary('t1');
    invalidPath.workspace_files[0].path = '/etc/passwd';
    assert.throws(
      () => assertTaskBoundaryV1(invalidPath),
      /must be relative to \/workspace/,
    );
  });

  test('taskBoundariesToJsonl serializes validated boundaries', () => {
    const jsonl = taskBoundariesToJsonl([makeTaskBoundary('t1'), makeTaskBoundary('t2')]);
    const lines = jsonl.trim().split('\n');
    assert.equal(lines.length, 2);
    const parsed = JSON.parse(lines[0]) as TaskBoundaryV1;
    assert.equal(parsed.schema_version, 'task_boundary_v1');
    assert.equal(parsed.task.id, 't1');
  });
});

describe('OutcomeMapper', () => {
  const trialOutput: TrialOutput = {
    schema_version: 'trial_output_v1',
    ids: {
      run_id: 'run_1',
      trial_id: 'trial_1',
      variant_id: 'baseline',
      task_id: 'task_1',
      repl_idx: 0,
    },
    outcome: 'success',
    metrics: { accuracy: 0.9 },
    objective: { name: 'accuracy', value: 0.9, direction: 'maximize' },
    artifacts: [{ path: '/out/report.json' }],
    checkpoints: [{ path: '/state/cp_1.json', logical_name: 'after_step_1', step: 1 }],
  };

  const runEvents: HookEvent[] = [
    {
      hooks_schema_version: 'hook_events_v1',
      event_type: 'agent_step_start',
      ts: '2026-02-12T00:00:00.000Z',
      seq: 0,
      ids: trialOutput.ids,
      step_index: 0,
    },
  ];

  test('createOutcomeBoundary creates runner-emitted shape for user mapping', () => {
    const boundary = createOutcomeBoundary(trialOutput, runEvents);
    assert.equal(boundary.schema_version, 'outcome_boundary_v1');
    assert.equal(boundary.result_summary.outcome, 'success');
    assert.equal(boundary.run_events.length, 1);
    assert.equal(boundary.run_events[0].event_type, 'agent_step_start');
  });

  test('mapOutcome supports sync user mappers', async () => {
    const mapper: OutcomeMapper<{ passed: boolean; calls: number }> = {
      map(boundary) {
        return {
          passed: boundary.result_summary.outcome === 'success',
          calls: boundary.run_events.length,
        };
      },
    };
    const mapped = await mapOutcome(createOutcomeBoundary(trialOutput, runEvents), mapper);
    assert.deepEqual(mapped, { passed: true, calls: 1 });
  });

  test('mapOutcome supports async user mappers', async () => {
    const mapper: OutcomeMapper<string> = {
      async map(boundary) {
        return `${boundary.result_summary.ids.trial_id}:${boundary.result_summary.outcome}`;
      },
    };
    const mapped = await mapOutcome(createOutcomeBoundary(trialOutput, runEvents), mapper);
    assert.equal(mapped, 'trial_1:success');
  });
});
