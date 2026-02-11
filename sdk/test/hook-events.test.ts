import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test, { describe } from 'node:test';

import type { HookEvent } from '../src/hook-events.js';

// ---------------------------------------------------------------------------
// Fixtures — shaped by hook_events_v1.jsonschema, NOT by SDK types.
//
// Each line is valid against the JSON schema. If the SDK types drift,
// the runtime assertions below will fail.
// ---------------------------------------------------------------------------

/** Real runner output from an actual trial run. */
const REAL_EVENTS_PATH = resolve(
  import.meta.dirname, '../../../.lab/runs/run_20260206_021411/trials/trial_1/workspace/harness_events.jsonl',
);

/**
 * Schema-derived JSONL covering all 6 event types with all optional fields.
 * Values match the schema constraints exactly:
 *   - control_version: string matching ^sha256:[0-9a-f]{64}$
 *   - action_observed / action_taken: enum ["continue", "stop", "checkpoint"]
 *   - outcome.status: enum ["ok", "error"]
 *   - timing fields: integers >= 0
 */
const SYNTHETIC_EVENTS = [
  // agent_step_start — required: event_type, step_index
  `{"hooks_schema_version":"hook_events_v1","event_type":"agent_step_start","ts":"2026-02-11T12:00:00Z","seq":1,"ids":{"run_id":"run_001","trial_id":"trial_001","variant_id":"baseline","task_id":"task_001","repl_idx":0},"step_index":0}`,

  // agent_step_end — required: event_type, step_index; optional: budgets
  `{"hooks_schema_version":"hook_events_v1","event_type":"agent_step_end","ts":"2026-02-11T12:00:01Z","seq":2,"ids":{"run_id":"run_001","trial_id":"trial_001","variant_id":"baseline","task_id":"task_001","repl_idx":0},"step_index":0,"budgets":{"steps":1,"tokens_in":1500,"tokens_out":200,"tool_calls":1}}`,

  // model_call_end — required: event_type, call_id, outcome; optional: turn_index, model, usage, timing, attempt_index
  `{"hooks_schema_version":"hook_events_v1","event_type":"model_call_end","ts":"2026-02-11T12:00:02Z","seq":3,"ids":{"run_id":"run_001","trial_id":"trial_001","variant_id":"baseline","task_id":"task_001","repl_idx":0},"step_index":0,"call_id":"call_001","outcome":{"status":"ok"},"turn_index":0,"model":{"identity":"gpt-4o","params_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"usage":{"tokens_in":1000,"tokens_out":200},"timing":{"queue_wait_ms":50,"duration_ms":800},"attempt_index":0}`,

  // tool_call_end — required: event_type, call_id, tool, outcome; optional: timing, attempt_index
  `{"hooks_schema_version":"hook_events_v1","event_type":"tool_call_end","ts":"2026-02-11T12:00:03Z","seq":4,"ids":{"run_id":"run_001","trial_id":"trial_001","variant_id":"baseline","task_id":"task_001","repl_idx":0},"step_index":0,"call_id":"tc_001","tool":{"name":"bash","version":"1.0.0"},"outcome":{"status":"error","error_type":"timeout","message":"killed after 30s"},"timing":{"duration_ms":30000},"attempt_index":1}`,

  // control_ack — required: event_type, step_index, control_version (STRING), action_observed (ENUM)
  `{"hooks_schema_version":"hook_events_v1","event_type":"control_ack","ts":"2026-02-11T12:00:04Z","seq":5,"ids":{"run_id":"run_001","trial_id":"trial_001","variant_id":"baseline","task_id":"task_001","repl_idx":0},"step_index":0,"control_version":"sha256:9e13783abffeb30dc804e67cd3d652b956758c47377267b26422567a6fb0ff95","control_seq":1,"action_observed":"checkpoint","action_taken":"checkpoint","reason":"user_requested"}`,

  // error — required: event_type, message; optional: error_type, stack
  `{"hooks_schema_version":"hook_events_v1","event_type":"error","ts":"2026-02-11T12:00:05Z","seq":6,"ids":{"run_id":"run_001","trial_id":"trial_001","variant_id":"baseline","task_id":"task_001","repl_idx":0},"message":"API rate limit exceeded","error_type":"rate_limit","stack":"at callModel() line 99"}`,
];

const VALID_ACTIONS = ['continue', 'stop', 'checkpoint'];

function parseEvents(lines: string[]): unknown[] {
  return lines.map((line) => JSON.parse(line));
}

// ---------------------------------------------------------------------------
// Schema conformance: field-level runtime type assertions
// ---------------------------------------------------------------------------

/** Validate base fields present on every event type per schema. */
function assertBaseFields(raw: Record<string, unknown>, label: string): void {
  assert.equal(raw.hooks_schema_version, 'hook_events_v1', `${label}: hooks_schema_version`);
  assert.equal(typeof raw.event_type, 'string', `${label}: event_type is string`);
  assert.equal(typeof raw.ts, 'string', `${label}: ts is string`);
  assert.equal(typeof raw.seq, 'number', `${label}: seq is number`);
  assert.ok(Number.isInteger(raw.seq), `${label}: seq is integer`);
  assert.equal(typeof raw.ids, 'object', `${label}: ids is object`);

  const ids = raw.ids as Record<string, unknown>;
  assert.equal(typeof ids.run_id, 'string', `${label}: ids.run_id`);
  assert.equal(typeof ids.trial_id, 'string', `${label}: ids.trial_id`);
  assert.equal(typeof ids.variant_id, 'string', `${label}: ids.variant_id`);
  assert.equal(typeof ids.task_id, 'string', `${label}: ids.task_id`);
  assert.equal(typeof ids.repl_idx, 'number', `${label}: ids.repl_idx`);
}

describe('HookEvent schema conformance — synthetic fixtures', () => {
  const events = parseEvents(SYNTHETIC_EVENTS);

  test('all 6 event types have valid base fields', () => {
    for (const raw of events) {
      const r = raw as Record<string, unknown>;
      assertBaseFields(r, r.event_type as string);
    }
  });

  test('agent_step_start — step_index is integer', () => {
    const raw = events[0] as Record<string, unknown>;
    assert.equal(raw.event_type, 'agent_step_start');
    assert.equal(typeof raw.step_index, 'number');
    assert.ok(Number.isInteger(raw.step_index));
  });

  test('agent_step_end — budgets fields are all integers', () => {
    const raw = events[1] as Record<string, unknown>;
    assert.equal(raw.event_type, 'agent_step_end');
    assert.equal(typeof raw.step_index, 'number');

    const budgets = raw.budgets as Record<string, unknown>;
    for (const field of ['steps', 'tokens_in', 'tokens_out', 'tool_calls']) {
      assert.equal(typeof budgets[field], 'number', `budgets.${field} is number`);
      assert.ok(Number.isInteger(budgets[field]), `budgets.${field} is integer`);
    }
  });

  test('model_call_end — outcome.status is enum, usage/timing are integers', () => {
    const raw = events[2] as Record<string, unknown>;
    assert.equal(raw.event_type, 'model_call_end');
    assert.equal(typeof raw.call_id, 'string');

    const outcome = raw.outcome as Record<string, unknown>;
    assert.ok(outcome.status === 'ok' || outcome.status === 'error');

    const model = raw.model as Record<string, unknown>;
    assert.equal(typeof model.identity, 'string');
    assert.equal(typeof model.params_digest, 'string');
    assert.ok((model.params_digest as string).startsWith('sha256:'));

    const usage = raw.usage as Record<string, unknown>;
    assert.equal(typeof usage.tokens_in, 'number');
    assert.equal(typeof usage.tokens_out, 'number');

    const timing = raw.timing as Record<string, unknown>;
    assert.equal(typeof timing.duration_ms, 'number');
    assert.equal(typeof timing.queue_wait_ms, 'number');
  });

  test('tool_call_end — tool.name is string, outcome.status is enum', () => {
    const raw = events[3] as Record<string, unknown>;
    assert.equal(raw.event_type, 'tool_call_end');
    assert.equal(typeof raw.call_id, 'string');

    const tool = raw.tool as Record<string, unknown>;
    assert.equal(typeof tool.name, 'string');
    assert.equal(typeof tool.version, 'string');

    const outcome = raw.outcome as Record<string, unknown>;
    assert.ok(outcome.status === 'ok' || outcome.status === 'error');
    assert.equal(typeof outcome.error_type, 'string');
    assert.equal(typeof outcome.message, 'string');
  });

  test('control_ack — control_version is STRING (sha256 digest), action_observed is enum', () => {
    const raw = events[4] as Record<string, unknown>;
    assert.equal(raw.event_type, 'control_ack');

    // THE BUG: old SDK had control_version: number. Schema says string.
    assert.equal(typeof raw.control_version, 'string', 'control_version must be string, not number');
    assert.ok(
      (raw.control_version as string).startsWith('sha256:'),
      'control_version must match ^sha256:[0-9a-f]{64}$',
    );
    assert.equal((raw.control_version as string).length, 7 + 64, 'sha256: prefix + 64 hex chars');

    // action_observed / action_taken must be from the enum
    assert.ok(VALID_ACTIONS.includes(raw.action_observed as string),
      `action_observed "${raw.action_observed}" not in ${JSON.stringify(VALID_ACTIONS)}`);
    assert.ok(VALID_ACTIONS.includes(raw.action_taken as string),
      `action_taken "${raw.action_taken}" not in ${JSON.stringify(VALID_ACTIONS)}`);

    assert.equal(typeof raw.control_seq, 'number');
    assert.equal(typeof raw.reason, 'string');
  });

  test('error — message is string, error_type and stack are optional strings', () => {
    const raw = events[5] as Record<string, unknown>;
    assert.equal(raw.event_type, 'error');
    assert.equal(typeof raw.message, 'string');
    assert.equal(typeof raw.error_type, 'string');
    assert.equal(typeof raw.stack, 'string');
  });
});

// ---------------------------------------------------------------------------
// Discriminated union: cast through HookEvent, narrow, access typed fields
// ---------------------------------------------------------------------------
describe('HookEvent discriminated union narrowing', () => {
  const events = parseEvents(SYNTHETIC_EVENTS) as HookEvent[];

  test('narrowing gives typed access to each event type', () => {
    for (const e of events) {
      switch (e.event_type) {
        case 'agent_step_start':
          assert.equal(typeof e.step_index, 'number');
          break;
        case 'agent_step_end':
          assert.equal(typeof e.step_index, 'number');
          if (e.budgets) {
            assert.equal(typeof e.budgets.steps, 'number');
          }
          break;
        case 'model_call_end':
          assert.equal(typeof e.call_id, 'string');
          assert.equal(typeof e.outcome.status, 'string');
          break;
        case 'tool_call_end':
          assert.equal(typeof e.tool.name, 'string');
          assert.equal(typeof e.outcome.status, 'string');
          break;
        case 'control_ack':
          // This is the key assertion: control_version accessed as string
          assert.equal(typeof e.control_version, 'string');
          assert.ok(VALID_ACTIONS.includes(e.action_observed));
          break;
        case 'error':
          assert.equal(typeof e.message, 'string');
          break;
      }
    }
  });
});

// ---------------------------------------------------------------------------
// Conformance against real runner output
// ---------------------------------------------------------------------------
describe('HookEvent real runner data', () => {
  let realEvents: unknown[] = [];
  let available = false;

  try {
    const lines = readFileSync(REAL_EVENTS_PATH, 'utf8')
      .split('\n')
      .filter((l) => l.trim().length > 0);
    realEvents = lines.map((l) => JSON.parse(l));
    available = true;
  } catch {
    // Run artifacts not present
  }

  test('all real events have valid base fields', { skip: !available && 'no run artifacts' }, () => {
    for (const raw of realEvents) {
      const r = raw as Record<string, unknown>;
      assertBaseFields(r, `seq=${r.seq}`);
    }
  });

  test('real control_ack has string control_version and enum actions', { skip: !available && 'no run artifacts' }, () => {
    const acks = realEvents.filter((e) => (e as Record<string, unknown>).event_type === 'control_ack');
    assert.ok(acks.length > 0, 'expected at least one control_ack event in real data');

    for (const raw of acks) {
      const r = raw as Record<string, unknown>;
      assert.equal(typeof r.control_version, 'string',
        'real control_ack: control_version must be string');
      assert.ok((r.control_version as string).startsWith('sha256:'),
        'real control_ack: control_version must start with sha256:');
      assert.ok(VALID_ACTIONS.includes(r.action_observed as string),
        `real control_ack: action_observed "${r.action_observed}" not in enum`);
      if (r.action_taken !== undefined) {
        assert.ok(VALID_ACTIONS.includes(r.action_taken as string),
          `real control_ack: action_taken "${r.action_taken}" not in enum`);
      }
    }
  });

  test('real model_call_end has string call_id and valid outcome', { skip: !available && 'no run artifacts' }, () => {
    const calls = realEvents.filter((e) => (e as Record<string, unknown>).event_type === 'model_call_end');
    for (const raw of calls) {
      const r = raw as Record<string, unknown>;
      assert.equal(typeof r.call_id, 'string');
      const outcome = r.outcome as Record<string, unknown>;
      assert.ok(outcome.status === 'ok' || outcome.status === 'error');
    }
  });
});
