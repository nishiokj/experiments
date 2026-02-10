#!/usr/bin/env node
const fs = require('fs');
const crypto = require('crypto');

function sha256Bytes(buf) {
  return 'sha256:' + crypto.createHash('sha256').update(buf).digest('hex');
}

function readJson(path) {
  return JSON.parse(fs.readFileSync(path, 'utf8'));
}

function writeJson(path, obj) {
  fs.writeFileSync(path, JSON.stringify(obj, null, 2));
}

function appendJsonl(path, obj) {
  fs.appendFileSync(path, JSON.stringify(obj) + '\n');
}

function nowIso() {
  return new Date().toISOString();
}

function main() {
  const inputPath = process.env.AGENTLAB_TRIAL_INPUT || 'trial_input.json';
  const outputPath = process.env.AGENTLAB_TRIAL_OUTPUT || 'trial_output.json';

  const ti = readJson(inputPath);
  const ids = ti.ids;
  const integration = (ti.design && ti.design.integration_level) || 'cli_basic';

  // Minimal behavior: success if prompt is present.
  const prompt = ti.task && ti.task.input && ti.task.input.prompt;
  const outcome = prompt ? 'success' : 'failure';

  // Emit manifest + hooks if requested.
  if (integration !== 'cli_basic') {
    const manifest = {
      schema_version: 'harness_manifest_v1',
      created_at: nowIso(),
      integration_level: integration,
      harness: {
        name: 'agentlab_demo_node',
        version: '0.1.0',
        entry_command: ['node', './agentlab_demo_harness.js', 'run'],
      },
      step: { semantics: 'decision_cycle' },
      control_plane: { mode: 'file', path: '/state/lab_control.json' },
    };
    if (integration === 'cli_events') {
      manifest.hooks = {
        schema_version: 'hook_events_v1',
        events_path: '/out/harness_events.jsonl',
        header_event_emitted: false,
      };
    }
    writeJson('harness_manifest.json', manifest);
  }

  if (integration === 'cli_events') {
    const eventsPath = 'harness_events.jsonl';
    const baseEvent = (event_type, seq, step_index) => ({
      hooks_schema_version: 'hook_events_v1',
      event_type,
      ts: nowIso(),
      seq,
      ids: {
        run_id: ids.run_id,
        trial_id: ids.trial_id,
        variant_id: ids.variant_id,
        task_id: ids.task_id,
        repl_idx: ids.repl_idx,
      },
      step_index,
    });

    appendJsonl(eventsPath, baseEvent('agent_step_start', 1, 0));

    // model_call_end as a universal "turn" signal
    appendJsonl(eventsPath, {
      ...baseEvent('model_call_end', 2, 0),
      call_id: 'call_1',
      outcome: { status: 'ok' },
      usage: { tokens_in: 0, tokens_out: 0 },
      timing: { duration_ms: 1 },
    });

    appendJsonl(eventsPath, baseEvent('agent_step_end', 3, 0));

    // Control plane ack.
    const cpPath = ti.runtime && ti.runtime.control_plane && ti.runtime.control_plane.path;
    let cpBytes = Buffer.from('{"action":"continue"}');
    if (cpPath && fs.existsSync(cpPath)) {
      cpBytes = fs.readFileSync(cpPath);
    }
    const controlVersion = sha256Bytes(cpBytes);

    appendJsonl(eventsPath, {
      ...baseEvent('control_ack', 4, 0),
      control_version: controlVersion,
      action_observed: 'continue',
      action_taken: 'continue',
    });
  }

  const out = {
    schema_version: 'trial_output_v1',
    ids,
    outcome,
    metrics: { latency_ms: 1 },
  };
  writeJson(outputPath, out);
}

main();
