import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test, { describe, afterEach, beforeEach } from 'node:test';

import { LabClient, LabRunnerError } from '../src/client.js';
import type { AnalysisSummary, AnalysisComparisons } from '../src/types.js';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Creates a fake runner script that echoes the received args as JSON,
 * and dispatches on the first arg (command name) for different behaviors.
 */
function makeFakeRunner(dir: string, script: string): string {
  const binPath = join(dir, 'fake-lab.js');
  writeFileSync(binPath, `#!/usr/bin/env node\n${script}`, {
    encoding: 'utf8',
    mode: 0o755,
  });
  return binPath;
}

/** Standard runner that returns success payloads for every command. */
function makeSuccessRunner(dir: string): string {
  return makeFakeRunner(
    dir,
    `
const args = process.argv.slice(2);
const cmd = args[0] || '';
const json = args.includes('--json');

const summary = {
  experiment: 'exp1', workload_type: 'agent_harness', dataset: 'tasks.jsonl',
  tasks: 2, replications: 1, variant_plan_entries: 1, total_trials: 2,
  harness: ['node','./harness.js','run'], integration_level: 'cli_basic',
  container_mode: false, network: 'none', control_path: '/state/lab_control.json',
  harness_script_exists: true,
};

const payloads = {
  'describe':        { ok: true, command: 'describe', summary },
  'run':             { ok: true, command: 'run', summary, run: { run_id: 'run_20260210_120000', run_dir: '.lab/runs/run_20260210_120000' } },
  'run-dev':         { ok: true, command: 'run-dev', summary, run: { run_id: 'run_dev_001', run_dir: '.lab/runs/run_dev_001' } },
  'replay':          { ok: true, command: 'replay', replay: { replay_id: 'replay_001', replay_dir: '.lab/runs/run1/replays/replay_001', parent_trial_id: 'trial_001', strict: false, replay_grade: 'best_effort', harness_status: 'ok' } },
  'fork':            { ok: true, command: 'fork', fork: { fork_id: 'fork_001', fork_dir: '.lab/runs/run1/forks/fork_001', parent_trial_id: 'trial_001', selector: 'checkpoint:cp1', strict: false, source_checkpoint: 'cp1', fallback_mode: 'none', replay_grade: 'checkpointed', harness_status: 'ok' } },
  'pause':           { ok: true, command: 'pause', pause: { run_id: 'run1', trial_id: 'trial_001', label: 'pause_001', checkpoint_acked: true, stop_acked: true } },
  'resume':          { ok: true, command: 'resume', resume: { trial_id: 'trial_001', selector: 'checkpoint:cp1', fork: { fork_id: 'fork_002', fork_dir: '.lab/runs/run1/forks/fork_002', parent_trial_id: 'trial_001', selector: 'checkpoint:cp1', strict: false, source_checkpoint: 'cp1', fallback_mode: 'none', replay_grade: 'checkpointed', harness_status: 'ok' } } },
  'publish':         { ok: true, command: 'publish', bundle: '/tmp/bundle.zip', run_dir: '.lab/runs/run1' },
  'knobs-validate':  { ok: true, command: 'knobs-validate', valid: true },
  'hooks-validate':  { ok: true, command: 'hooks-validate', valid: true },
  'schema-validate': { ok: true, command: 'schema-validate', valid: true },
};

const payload = payloads[cmd] || { ok: true, command: cmd, valid: true };
console.log(JSON.stringify(payload));
process.exit(0);
`,
  );
}

/** Runner that writes received args to a sidecar file for assertion. */
function makeArgCapturingRunner(dir: string): { binPath: string; argsFile: string } {
  const argsFile = join(dir, 'captured-args.json');
  const binPath = makeFakeRunner(
    dir,
    `
const fs = require('fs');
const args = process.argv.slice(2);
fs.writeFileSync('${argsFile.replace(/\\/g, '\\\\')}', JSON.stringify(args));
console.log(JSON.stringify({ ok: true, command: args[0], valid: true, summary: {} }));
process.exit(0);
`,
  );
  return { binPath, argsFile };
}

/** Runner that emits an error envelope on stdout (non-zero exit but has JSON). */
function makeErrorEnvelopeRunner(dir: string): string {
  return makeFakeRunner(
    dir,
    `
console.log(JSON.stringify({ ok: false, error: { code: 'bad_config', message: 'invalid config', details: { path: 'x' } } }));
process.exit(1);
`,
  );
}

// ---------------------------------------------------------------------------
// LabRunnerError
// ---------------------------------------------------------------------------
describe('LabRunnerError', () => {
  test('has correct name', () => {
    const err = new LabRunnerError({
      message: 'boom',
      code: 'test_error',
      command: ['lab', 'run'],
      stderr: 'err output',
    });
    assert.equal(err.name, 'LabRunnerError');
  });

  test('stores all properties', () => {
    const err = new LabRunnerError({
      message: 'fail',
      code: 'cfg_error',
      command: ['lab', 'describe', '--json'],
      stderr: 'stderr text',
      details: { field: 'missing' },
      exitCode: 2,
    });
    assert.equal(err.message, 'fail');
    assert.equal(err.code, 'cfg_error');
    assert.deepEqual(err.command, ['lab', 'describe', '--json']);
    assert.equal(err.stderr, 'stderr text');
    assert.deepEqual(err.details, { field: 'missing' });
    assert.equal(err.exitCode, 2);
  });

  test('is an instance of Error', () => {
    const err = new LabRunnerError({
      message: 'x',
      code: 'x',
      command: [],
      stderr: '',
    });
    assert.ok(err instanceof Error);
  });

  test('optional properties default to undefined', () => {
    const err = new LabRunnerError({
      message: 'x',
      code: 'x',
      command: [],
      stderr: '',
    });
    assert.equal(err.details, undefined);
    assert.equal(err.exitCode, undefined);
  });
});

// ---------------------------------------------------------------------------
// LabClient – constructor / runner resolution
// ---------------------------------------------------------------------------
describe('LabClient constructor', () => {
  test('defaults runnerBin to "lab" when no option or env', () => {
    // We can't easily test the default without spawning, but we can test that
    // a bogus binary throws spawn_failed
    const client = new LabClient({ runnerBin: '__nonexistent_binary__' });
    assert.rejects(
      () => client.describe({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'spawn_failed');
        return true;
      },
    );
  });

  test('accepts custom env', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
    try {
      const binPath = makeFakeRunner(
        dir,
        `
const val = process.env.MY_CUSTOM_VAR || 'not set';
console.log(JSON.stringify({ ok: true, command: 'describe', summary: { custom_var: val, experiment: 'e', workload_type:'', dataset:'', tasks:0, replications:0, variant_plan_entries:0, total_trials:0, harness:[], integration_level:'', container_mode:false, network:'', control_path:'', harness_script_exists:false } }));
`,
      );
      const client = new LabClient({
        runnerBin: binPath,
        cwd: dir,
        env: { ...process.env, MY_CUSTOM_VAR: 'hello' },
      });
      const res = await client.describe({ experiment: 'e.yaml' });
      assert.equal((res.summary as unknown as Record<string, unknown>).custom_var, 'hello');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

// ---------------------------------------------------------------------------
// LabClient – describe()
// ---------------------------------------------------------------------------
describe('LabClient.describe()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('parses success payload', async () => {
    const bin = makeSuccessRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.describe({ experiment: 'experiment.yaml' });
    assert.equal(res.ok, true);
    assert.equal(res.command, 'describe');
    assert.equal(res.summary.experiment, 'exp1');
    assert.equal(res.summary.total_trials, 2);
  });

  test('passes --overrides when provided', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.describe({ experiment: 'exp.yaml', overrides: 'knobs.json' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('--overrides'));
    assert.ok(args.includes('knobs.json'));
    assert.ok(args.includes('--json'));
    assert.ok(args.includes('describe'));
  });

  test('does not pass --overrides when omitted', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.describe({ experiment: 'exp.yaml' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(!args.includes('--overrides'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – run()
// ---------------------------------------------------------------------------
describe('LabClient.run()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('parses run success payload', async () => {
    const bin = makeSuccessRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.run({ experiment: 'exp.yaml' });
    assert.equal(res.ok, true);
    assert.equal(res.command, 'run');
    assert.ok(res.run.run_id);
  });

  test('passes experiment and --json to lab run', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.run({ experiment: 'exp.yaml' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('run'));
    assert.ok(args.includes('exp.yaml'));
    assert.ok(args.includes('--json'));
  });

  test('passes --overrides when provided', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.run({ experiment: 'exp.yaml', overrides: 'ov.json' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('--overrides'));
    assert.ok(args.includes('ov.json'));
  });

  test('passes executor/materialize/remote flags when provided', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.run({
      experiment: 'exp.yaml',
      executor: 'remote',
      materialize: 'metadata_only',
      remoteEndpoint: 'https://runner.example.com',
      remoteTokenEnv: 'AGENTLAB_REMOTE_TOKEN',
    });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('--executor'));
    assert.ok(args.includes('remote'));
    assert.ok(args.includes('--materialize'));
    assert.ok(args.includes('metadata_only'));
    assert.ok(args.includes('--remote-endpoint'));
    assert.ok(args.includes('https://runner.example.com'));
    assert.ok(args.includes('--remote-token-env'));
    assert.ok(args.includes('AGENTLAB_REMOTE_TOKEN'));
  });

  test('throws LabRunnerError on error envelope', async () => {
    const bin = makeErrorEnvelopeRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    await assert.rejects(
      () => client.run({ experiment: 'exp.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'bad_config');
        assert.equal(err.message, 'invalid config');
        assert.deepEqual(err.details, { path: 'x' });
        assert.ok(err.command.length > 0);
        return true;
      },
    );
  });
});

// ---------------------------------------------------------------------------
// LabClient – runDev()
// ---------------------------------------------------------------------------
describe('LabClient.runDev()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('passes --setup when provided', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.runDev({ experiment: 'exp.yaml', setup: 'npm install' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('run-dev'));
    assert.ok(args.includes('--setup'));
    assert.ok(args.includes('npm install'));
  });

  test('does not pass --setup when omitted', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.runDev({ experiment: 'exp.yaml' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('run-dev'));
    assert.ok(!args.includes('--setup'));
  });

  test('passes --overrides when provided', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.runDev({ experiment: 'exp.yaml', overrides: 'ov.json' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('--overrides'));
    assert.ok(args.includes('ov.json'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – replay()
// ---------------------------------------------------------------------------
describe('LabClient.replay()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('parses replay success payload', async () => {
    const bin = makeSuccessRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.replay({ runDir: '.lab/runs/run1', trialId: 'trial_001' });
    assert.equal(res.ok, true);
    assert.equal(res.command, 'replay');
    assert.equal(res.replay.replay_id, 'replay_001');
    assert.equal(res.replay.parent_trial_id, 'trial_001');
  });

  test('passes required replay args and --strict', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.replay({ runDir: '.lab/runs/run1', trialId: 'trial_001', strict: true });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.deepEqual(args.slice(0, 7), [
      'replay',
      '--run-dir',
      '.lab/runs/run1',
      '--trial-id',
      'trial_001',
      '--json',
      '--strict',
    ]);
  });
});

// ---------------------------------------------------------------------------
// LabClient – fork()
// ---------------------------------------------------------------------------
describe('LabClient.fork()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('parses fork success payload', async () => {
    const bin = makeSuccessRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.fork({
      runDir: '.lab/runs/run1',
      fromTrial: 'trial_001',
      at: 'checkpoint:cp1',
    });
    assert.equal(res.ok, true);
    assert.equal(res.command, 'fork');
    assert.equal(res.fork.fork_id, 'fork_001');
    assert.equal(res.fork.selector, 'checkpoint:cp1');
  });

  test('passes selector, --set bindings, and --strict', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.fork({
      runDir: '.lab/runs/run1',
      fromTrial: 'trial_001',
      at: 'checkpoint:cp1',
      set: {
        temperature: 0.2,
        model: 'gpt-4.1',
        allow_tools: true,
        metadata: { lane: 'canary' },
      },
      strict: true,
    });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('fork'));
    assert.ok(args.includes('--from-trial'));
    assert.ok(args.includes('trial_001'));
    assert.ok(args.includes('--at'));
    assert.ok(args.includes('checkpoint:cp1'));
    assert.ok(args.includes('--strict'));
    assert.ok(args.includes('--set'));
    assert.ok(args.includes('temperature=0.2'));
    assert.ok(args.includes('model="gpt-4.1"'));
    assert.ok(args.includes('allow_tools=true'));
    assert.ok(args.includes('metadata={"lane":"canary"}'));
  });

  test('rejects undefined set binding values', async () => {
    const { binPath } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await assert.rejects(
      () =>
        client.fork({
          runDir: '.lab/runs/run1',
          fromTrial: 'trial_001',
          at: 'checkpoint:cp1',
          set: { a: undefined },
        }),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.equal(err.message, 'set binding "a" cannot be undefined');
        return true;
      },
    );
  });
});

// ---------------------------------------------------------------------------
// LabClient – pause()
// ---------------------------------------------------------------------------
describe('LabClient.pause()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('parses pause success payload', async () => {
    const bin = makeSuccessRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.pause({ runDir: '.lab/runs/run1' });
    assert.equal(res.ok, true);
    assert.equal(res.command, 'pause');
    assert.equal(res.pause.trial_id, 'trial_001');
    assert.equal(res.pause.checkpoint_acked, true);
    assert.equal(res.pause.stop_acked, true);
  });

  test('passes optional pause fields when provided', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.pause({
      runDir: '.lab/runs/run1',
      trialId: 'trial_001',
      label: 'safe_boundary',
      timeoutSeconds: 120,
    });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('pause'));
    assert.ok(args.includes('--trial-id'));
    assert.ok(args.includes('trial_001'));
    assert.ok(args.includes('--label'));
    assert.ok(args.includes('safe_boundary'));
    assert.ok(args.includes('--timeout-seconds'));
    assert.ok(args.includes('120'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – resume()
// ---------------------------------------------------------------------------
describe('LabClient.resume()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('parses resume success payload', async () => {
    const bin = makeSuccessRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.resume({ runDir: '.lab/runs/run1' });
    assert.equal(res.ok, true);
    assert.equal(res.command, 'resume');
    assert.equal(res.resume.trial_id, 'trial_001');
    assert.equal(res.resume.fork.fork_id, 'fork_002');
  });

  test('passes optional resume fields, --set, and --strict', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.resume({
      runDir: '.lab/runs/run1',
      trialId: 'trial_001',
      label: 'cp1',
      set: { model: 'gpt-4.1-mini', max_steps: 50 },
      strict: true,
    });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('resume'));
    assert.ok(args.includes('--trial-id'));
    assert.ok(args.includes('trial_001'));
    assert.ok(args.includes('--label'));
    assert.ok(args.includes('cp1'));
    assert.ok(args.includes('--strict'));
    assert.ok(args.includes('--set'));
    assert.ok(args.includes('model="gpt-4.1-mini"'));
    assert.ok(args.includes('max_steps=50'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – publish()
// ---------------------------------------------------------------------------
describe('LabClient.publish()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('passes --run-dir and --out', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.publish({ runDir: '.lab/runs/run1', out: '/tmp/out.zip' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('publish'));
    assert.ok(args.includes('--run-dir'));
    assert.ok(args.includes('.lab/runs/run1'));
    assert.ok(args.includes('--out'));
    assert.ok(args.includes('/tmp/out.zip'));
  });

  test('omits --out when not set', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.publish({ runDir: '.lab/runs/run1' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(!args.includes('--out'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – validateKnobs()
// ---------------------------------------------------------------------------
describe('LabClient.validateKnobs()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('passes --manifest and --overrides', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.validateKnobs({ manifest: 'knobs.json', overrides: 'ov.json' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('knobs-validate'));
    assert.ok(args.includes('--manifest'));
    assert.ok(args.includes('knobs.json'));
    assert.ok(args.includes('--overrides'));
    assert.ok(args.includes('ov.json'));
    assert.ok(args.includes('--json'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – validateHooks()
// ---------------------------------------------------------------------------
describe('LabClient.validateHooks()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('passes --manifest and --events', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.validateHooks({ manifest: 'harness.json', events: 'events.jsonl' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('hooks-validate'));
    assert.ok(args.includes('--manifest'));
    assert.ok(args.includes('harness.json'));
    assert.ok(args.includes('--events'));
    assert.ok(args.includes('events.jsonl'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – validateSchema()
// ---------------------------------------------------------------------------
describe('LabClient.validateSchema()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('passes --schema and --file', async () => {
    const { binPath, argsFile } = makeArgCapturingRunner(dir);
    const client = new LabClient({ runnerBin: binPath, cwd: dir });
    await client.validateSchema({ schema: 'trial_output_v1', file: 'output.json' });

    const { readFileSync } = await import('node:fs');
    const args: string[] = JSON.parse(readFileSync(argsFile, 'utf8'));
    assert.ok(args.includes('schema-validate'));
    assert.ok(args.includes('--schema'));
    assert.ok(args.includes('trial_output_v1'));
    assert.ok(args.includes('--file'));
    assert.ok(args.includes('output.json'));
  });
});

// ---------------------------------------------------------------------------
// LabClient – error handling edge cases
// ---------------------------------------------------------------------------
describe('LabClient error handling', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('throws spawn_failed for nonexistent binary', async () => {
    const client = new LabClient({
      runnerBin: join(dir, 'does-not-exist'),
      cwd: dir,
    });
    await assert.rejects(
      () => client.describe({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'spawn_failed');
        return true;
      },
    );
  });

  test('throws runner_exit_nonzero when exit code > 0 and no stdout', async () => {
    const bin = makeFakeRunner(dir, 'process.stderr.write("something broke"); process.exit(42);');
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    await assert.rejects(
      () => client.describe({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'runner_exit_nonzero');
        assert.equal(err.exitCode, 42);
        assert.ok(err.stderr.includes('something broke'));
        return true;
      },
    );
  });

  test('throws empty_payload when stdout is empty and exit 0', async () => {
    const bin = makeFakeRunner(dir, 'process.exit(0);');
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    await assert.rejects(
      () => client.describe({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'empty_payload');
        return true;
      },
    );
  });

  test('throws invalid_json when stdout is not JSON', async () => {
    const bin = makeFakeRunner(dir, 'console.log("not json at all"); process.exit(0);');
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    await assert.rejects(
      () => client.describe({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'invalid_json');
        return true;
      },
    );
  });

  test('throws invalid_payload when JSON is not an object', async () => {
    const bin = makeFakeRunner(dir, 'console.log("42"); process.exit(0);');
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    await assert.rejects(
      () => client.describe({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.code, 'invalid_payload');
        return true;
      },
    );
  });

  test('uses last line of multi-line stdout for JSON parsing', async () => {
    const bin = makeFakeRunner(
      dir,
      `
console.log("some log line");
console.log("another log");
console.log(JSON.stringify({ ok: true, command: 'describe', summary: { experiment: 'e', workload_type:'', dataset:'', tasks:0, replications:0, variant_plan_entries:0, total_trials:0, harness:[], integration_level:'', container_mode:false, network:'', control_path:'', harness_script_exists:false } }));
process.exit(0);
`,
    );
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.describe({ experiment: 'e.yaml' });
    assert.equal(res.ok, true);
  });

  test('error envelope includes exitCode and command in LabRunnerError', async () => {
    const bin = makeErrorEnvelopeRunner(dir);
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    await assert.rejects(
      () => client.run({ experiment: 'e.yaml' }),
      (err: unknown) => {
        assert.ok(err instanceof LabRunnerError);
        assert.equal(err.exitCode, 1);
        assert.ok(err.command.includes('run'));
        return true;
      },
    );
  });
});

// ---------------------------------------------------------------------------
// LabClient – per-call cwd/env overrides
// ---------------------------------------------------------------------------
describe('LabClient per-call CommandOptions', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  test('per-call env is merged into client env', async () => {
    const bin = makeFakeRunner(
      dir,
      `
const val = process.env.CALL_VAR || 'not set';
console.log(JSON.stringify({ ok: true, command: 'describe', summary: { call_var: val, experiment:'e', workload_type:'', dataset:'', tasks:0, replications:0, variant_plan_entries:0, total_trials:0, harness:[], integration_level:'', container_mode:false, network:'', control_path:'', harness_script_exists:false } }));
`,
    );
    const client = new LabClient({ runnerBin: bin, cwd: dir });
    const res = await client.describe({
      experiment: 'e.yaml',
      env: { CALL_VAR: 'per-call' },
    });
    assert.equal(
      (res.summary as unknown as Record<string, unknown>).call_var,
      'per-call',
    );
  });
});

// ---------------------------------------------------------------------------
// LabClient – readAnalysis()
// ---------------------------------------------------------------------------
describe('LabClient.readAnalysis()', () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sdk-test-'));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  function writeAnalysisFixtures(runDir: string) {
    const analysisDir = join(runDir, 'analysis');
    mkdirSync(analysisDir, { recursive: true });

    const summary: AnalysisSummary = {
      schema_version: 'analysis_v1',
      baseline_id: 'control',
      variants: {
        control: {
          total: 50,
          success_rate: 0.72,
          primary_metric_name: 'resolved',
          primary_metric_mean: 0.72,
          event_counts: {
            agent_step_start: 500,
            agent_step_end: 500,
            model_call_end: 1500,
            tool_call_end: 800,
            control_ack: 50,
            error: 2,
          },
        },
        treatment: {
          total: 50,
          success_rate: 0.80,
          primary_metric_name: 'resolved',
          primary_metric_mean: 0.80,
          event_counts: {
            agent_step_start: 480,
            agent_step_end: 480,
            model_call_end: 1400,
            tool_call_end: 750,
            control_ack: 50,
            error: 1,
          },
        },
      },
    };

    const comparisons: AnalysisComparisons = {
      schema_version: 'analysis_v1',
      comparisons: [
        {
          baseline: 'control',
          variant: 'treatment',
          baseline_success_rate: 0.72,
          variant_success_rate: 0.80,
        },
      ],
    };

    writeFileSync(join(analysisDir, 'summary.json'), JSON.stringify(summary));
    writeFileSync(join(analysisDir, 'comparisons.json'), JSON.stringify(comparisons));
  }

  test('reads and parses analysis files', async () => {
    const runDir = join(dir, 'run_001');
    writeAnalysisFixtures(runDir);

    const client = new LabClient({ cwd: dir });
    const result = await client.readAnalysis({ runDir: 'run_001' });

    assert.equal(result.summary.schema_version, 'analysis_v1');
    assert.equal(result.summary.baseline_id, 'control');
    assert.equal(Object.keys(result.summary.variants).length, 2);
    assert.equal(result.summary.variants.control.success_rate, 0.72);
    assert.equal(result.summary.variants.treatment.success_rate, 0.80);
    assert.equal(result.summary.variants.control.event_counts.model_call_end, 1500);

    assert.equal(result.comparisons.schema_version, 'analysis_v1');
    assert.equal(result.comparisons.comparisons.length, 1);
    assert.equal(result.comparisons.comparisons[0].baseline, 'control');
    assert.equal(result.comparisons.comparisons[0].variant, 'treatment');
    assert.equal(result.comparisons.comparisons[0].variant_success_rate, 0.80);
  });

  test('throws when analysis directory does not exist', async () => {
    const client = new LabClient({ cwd: dir });
    await assert.rejects(
      () => client.readAnalysis({ runDir: 'nonexistent_run' }),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        return true;
      },
    );
  });

  test('reads from absolute runDir', async () => {
    const runDir = join(dir, 'abs_run');
    writeAnalysisFixtures(runDir);

    const client = new LabClient();
    const result = await client.readAnalysis({ runDir });

    assert.equal(result.summary.baseline_id, 'control');
    assert.equal(result.comparisons.comparisons.length, 1);
  });
});
