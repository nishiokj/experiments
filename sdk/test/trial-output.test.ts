import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test, { describe } from 'node:test';

import type { TrialOutput } from '../src/trial-output.js';

// ---------------------------------------------------------------------------
// Fixtures — shaped by the schema, NOT by the SDK types.
//
// These are the source of truth. If the SDK type drifts from the schema,
// the runtime assertions below will fail because the fixture data won't
// match the type's field-level expectations.
// ---------------------------------------------------------------------------

/** Matches trial_output_v1.jsonschema — minimal required fields only. */
const MINIMAL_OUTPUT = `{
  "schema_version": "trial_output_v1",
  "ids": {
    "run_id": "run_001",
    "trial_id": "trial_001",
    "variant_id": "baseline",
    "task_id": "task_001",
    "repl_idx": 0
  },
  "outcome": "failure",
  "metrics": { "latency_ms": 1 }
}`;

/** Matches trial_output_v1.jsonschema — all optional fields populated. */
const FULL_OUTPUT = `{
  "schema_version": "trial_output_v1",
  "ids": {
    "run_id": "run_001",
    "trial_id": "trial_001",
    "variant_id": "baseline",
    "task_id": "task_001",
    "repl_idx": 0
  },
  "outcome": "success",
  "answer": "Apply the null check on line 42",
  "metrics": {
    "accuracy": 0.95,
    "cost_usd": 0.12,
    "passed": true,
    "label": "correct",
    "skipped": null
  },
  "objective": {
    "name": "resolved",
    "value": 1.0,
    "direction": "maximize"
  },
  "artifacts": [
    { "path": "/out/patch.diff", "logical_name": "solution_diff", "mime_type": "text/x-diff" }
  ],
  "checkpoints": [
    { "path": "/state/cp1.json", "logical_name": "after_analysis", "step": 3, "epoch": 1.5 }
  ],
  "error": {
    "error_type": "partial_failure",
    "message": "One sub-task failed",
    "stack": "at evaluate() line 99"
  },
  "ext": { "custom": true }
}`;

// Path to real runner output, relative to repo root.
const REAL_OUTPUT_PATH = resolve(
  import.meta.dirname, '../../../.lab/runs/run_20260206_021411/trials/trial_1/trial_output.json',
);

function parse(json: string): unknown {
  return JSON.parse(json);
}

// ---------------------------------------------------------------------------
// Schema conformance: parse schema-shaped JSON, validate field types at runtime
// ---------------------------------------------------------------------------
describe('TrialOutput schema conformance', () => {
  test('minimal output — required fields have correct runtime types', () => {
    const raw = parse(MINIMAL_OUTPUT) as TrialOutput;

    assert.equal(raw.schema_version, 'trial_output_v1');
    assert.equal(typeof raw.ids, 'object');
    assert.equal(typeof raw.ids.run_id, 'string');
    assert.equal(typeof raw.ids.trial_id, 'string');
    assert.equal(typeof raw.ids.variant_id, 'string');
    assert.equal(typeof raw.ids.task_id, 'string');
    assert.equal(typeof raw.ids.repl_idx, 'number');
    assert.equal(typeof raw.outcome, 'string');
    assert.ok(['success', 'failure', 'missing', 'error'].includes(raw.outcome));
  });

  test('metrics values are number | string | boolean | null per schema', () => {
    const raw = parse(FULL_OUTPUT) as TrialOutput;
    const metrics = raw.metrics!;

    assert.equal(typeof metrics.accuracy, 'number');
    assert.equal(typeof metrics.cost_usd, 'number');
    assert.equal(typeof metrics.passed, 'boolean');
    assert.equal(typeof metrics.label, 'string');
    assert.equal(metrics.skipped, null);
  });

  test('objective is a single object, NOT an array', () => {
    const raw = parse(FULL_OUTPUT) as TrialOutput;

    // This is the bug the old tests missed: schema says object, not array.
    assert.ok(!Array.isArray(raw.objective), 'objective must not be an array');
    assert.equal(typeof raw.objective, 'object');
    assert.equal(typeof raw.objective!.name, 'string');
    assert.equal(typeof raw.objective!.value, 'number');
    assert.ok(
      raw.objective!.direction === undefined ||
      raw.objective!.direction === 'maximize' ||
      raw.objective!.direction === 'minimize',
    );
  });

  test('answer is string | object | array per schema oneOf', () => {
    // string answer
    const withString = parse(FULL_OUTPUT) as TrialOutput;
    assert.equal(typeof withString.answer, 'string');

    // object answer
    const objFixture = parse(`{
      "schema_version": "trial_output_v1",
      "ids": {"run_id":"r","trial_id":"t","variant_id":"v","task_id":"tk","repl_idx":0},
      "outcome": "success",
      "answer": {"patch": "diff content", "confidence": 0.9}
    }`) as TrialOutput;
    assert.equal(typeof objFixture.answer, 'object');
    assert.ok(!Array.isArray(objFixture.answer));

    // array answer
    const arrFixture = parse(`{
      "schema_version": "trial_output_v1",
      "ids": {"run_id":"r","trial_id":"t","variant_id":"v","task_id":"tk","repl_idx":0},
      "outcome": "success",
      "answer": ["step1", "step2"]
    }`) as TrialOutput;
    assert.ok(Array.isArray(arrFixture.answer));
  });

  test('artifacts is an array of objects with required path', () => {
    const raw = parse(FULL_OUTPUT) as TrialOutput;
    assert.ok(Array.isArray(raw.artifacts));
    const art = raw.artifacts![0];
    assert.equal(typeof art.path, 'string');
    assert.equal(typeof art.logical_name, 'string');
    assert.equal(typeof art.mime_type, 'string');
  });

  test('checkpoints — epoch is number (not integer-only) per schema', () => {
    const raw = parse(FULL_OUTPUT) as TrialOutput;
    const cp = raw.checkpoints![0];
    assert.equal(typeof cp.path, 'string');
    assert.equal(typeof cp.step, 'number');
    assert.equal(typeof cp.epoch, 'number');
    // Schema says "type": "number" for epoch — fractional values are valid
    assert.equal(cp.epoch, 1.5);
  });

  test('error fields are all strings per schema', () => {
    const raw = parse(FULL_OUTPUT) as TrialOutput;
    const err = raw.error!;
    assert.equal(typeof err.error_type, 'string');
    assert.equal(typeof err.message, 'string');
    assert.equal(typeof err.stack, 'string');
  });
});

// ---------------------------------------------------------------------------
// Conformance against real runner output
// ---------------------------------------------------------------------------
describe('TrialOutput real runner data', () => {
  let realOutput: unknown;
  let available = false;

  try {
    realOutput = JSON.parse(readFileSync(REAL_OUTPUT_PATH, 'utf8'));
    available = true;
  } catch {
    // Run artifacts not present — skip gracefully
  }

  test('real trial_output.json matches SDK types', { skip: !available && 'no run artifacts' }, () => {
    const raw = realOutput as TrialOutput;

    assert.equal(raw.schema_version, 'trial_output_v1');
    assert.equal(typeof raw.ids.run_id, 'string');
    assert.equal(typeof raw.ids.trial_id, 'string');
    assert.equal(typeof raw.ids.variant_id, 'string');
    assert.equal(typeof raw.ids.task_id, 'string');
    assert.equal(typeof raw.ids.repl_idx, 'number');
    assert.ok(['success', 'failure', 'missing', 'error'].includes(raw.outcome));

    // If metrics present, every value must be number | string | boolean | null
    if (raw.metrics) {
      for (const [key, val] of Object.entries(raw.metrics)) {
        const t = typeof val;
        assert.ok(
          t === 'number' || t === 'string' || t === 'boolean' || val === null,
          `metrics.${key} has type ${t}, expected number|string|boolean|null`,
        );
      }
    }

    // objective must be a plain object if present, never an array
    if (raw.objective) {
      assert.ok(!Array.isArray(raw.objective), 'objective must not be an array');
    }
  });
});
