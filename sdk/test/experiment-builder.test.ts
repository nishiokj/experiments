import assert from 'node:assert/strict';
import test, { describe } from 'node:test';

import { ExperimentBuilder, Metric } from '../src/experiment-builder.js';
import type { ExperimentSpec, MetricDef, GuardrailDef } from '../src/experiment-builder.js';

// Helper: create a fully configured builder that passes build() validation
function validBuilder(): ExperimentBuilder {
  return ExperimentBuilder.create('exp-1', 'My Experiment')
    .datasetJsonl('tasks.jsonl', { suiteId: 'suite', splitId: 'dev', limit: 50 })
    .harnessCli(['node', './harness.js', 'run'], { integrationLevel: 'cli_basic' });
}

// ---------------------------------------------------------------------------
// Structural defaults (fields that have reasonable defaults without setters)
// ---------------------------------------------------------------------------
describe('ExperimentBuilder structural defaults', () => {
  const spec = validBuilder().build();

  test('version is 0.3', () => {
    assert.equal(spec.version, '0.3');
  });

  test('experiment id and name are set', () => {
    assert.equal(spec.experiment.id, 'exp-1');
    assert.equal(spec.experiment.name, 'My Experiment');
  });

  test('description and owner are undefined by default', () => {
    assert.equal(spec.experiment.description, undefined);
    assert.equal(spec.experiment.owner, undefined);
  });

  test('default sanitization_profile is hermetic_functional_v2', () => {
    assert.equal(spec.design.sanitization_profile, 'hermetic_functional_v2');
  });

  test('default replications is 1', () => {
    assert.equal(spec.design.replications, 1);
  });

  test('default random_seed is 1', () => {
    assert.equal(spec.design.random_seed, 1);
  });

  test('default comparison is paired', () => {
    assert.equal(spec.design.comparison, 'paired');
  });

  test('default shuffle_tasks is true', () => {
    assert.equal(spec.design.shuffle_tasks, true);
  });

  test('default max_concurrency is 1', () => {
    assert.equal(spec.design.max_concurrency, 1);
  });

  test('default metrics is empty array', () => {
    assert.deepEqual(spec.metrics, []);
  });

  test('default baseline', () => {
    assert.equal(spec.baseline.variant_id, 'base');
    assert.deepEqual(spec.baseline.bindings, {});
  });

  test('default variant_plan is empty', () => {
    assert.deepEqual(spec.variant_plan, []);
  });

  test('default harness mode is cli', () => {
    assert.equal(spec.runtime.harness.mode, 'cli');
  });

  test('default input/output paths', () => {
    assert.equal(spec.runtime.harness.input_path, '/out/trial_input.json');
    assert.equal(spec.runtime.harness.output_path, '/out/trial_output.json');
  });

  test('default control plane', () => {
    assert.equal(spec.runtime.harness.control_plane.mode, 'file');
    assert.equal(spec.runtime.harness.control_plane.path, '/state/lab_control.json');
  });

  test('default sandbox is local mode', () => {
    assert.equal(spec.runtime.sandbox.mode, 'local');
  });

  test('default network is none', () => {
    assert.equal(spec.runtime.network.mode, 'none');
    assert.deepEqual(spec.runtime.network.allowed_hosts, []);
  });

  test('default validity flags', () => {
    assert.equal(spec.validity.fail_on_state_leak, true);
    assert.equal(spec.validity.fail_on_profile_invariant_violation, true);
  });
});

// ---------------------------------------------------------------------------
// build() validation
// ---------------------------------------------------------------------------
describe('ExperimentBuilder build() validation', () => {
  test('build() throws when no required fields are set', () => {
    assert.throws(
      () => ExperimentBuilder.create('e', 'n').build(),
      (err: Error) => {
        assert.ok(err.message.includes('ExperimentBuilder: required fields not set'));
        assert.ok(err.message.includes('dataset path'));
        assert.ok(err.message.includes('dataset suite_id'));
        assert.ok(err.message.includes('dataset split_id'));
        assert.ok(err.message.includes('dataset limit'));
        assert.ok(err.message.includes('harness command'));
        assert.ok(err.message.includes('harness integration_level'));
        // defaults should NOT be listed
        assert.ok(!err.message.includes('sanitization_profile'));
        assert.ok(!err.message.includes('replications'));
        assert.ok(!err.message.includes('random_seed'));
        return true;
      },
    );
  });

  test('build() throws listing only missing fields', () => {
    assert.throws(
      () =>
        ExperimentBuilder.create('e', 'n')
          .datasetJsonl('tasks.jsonl', { suiteId: 's', splitId: 'dev', limit: 10 })
          .build(),
      (err: Error) => {
        // harness command and integration_level still missing
        assert.ok(err.message.includes('harness command'));
        assert.ok(err.message.includes('harness integration_level'));
        // these should NOT be listed
        assert.ok(!err.message.includes('dataset path'));
        assert.ok(!err.message.includes('sanitization_profile'));
        assert.ok(!err.message.includes('replications'));
        assert.ok(!err.message.includes('random_seed'));
        return true;
      },
    );
  });

  test('build() succeeds with only dataset and harness set', () => {
    const spec = ExperimentBuilder.create('e', 'n')
      .datasetJsonl('tasks.jsonl', { suiteId: 's', splitId: 'dev', limit: 10 })
      .harnessCli(['node', './h.js'], { integrationLevel: 'cli_basic' })
      .build();
    assert.equal(spec.version, '0.3');
    assert.equal(spec.dataset.path, 'tasks.jsonl');
    assert.equal(spec.design.sanitization_profile, 'hermetic_functional_v2');
    assert.equal(spec.design.replications, 1);
    assert.equal(spec.design.random_seed, 1);
    assert.equal(spec.runtime.harness.integration_level, 'cli_basic');
  });

  test('toYaml() also validates', () => {
    assert.throws(
      () => ExperimentBuilder.create('e', 'n').toYaml(),
      (err: Error) => err.message.includes('required fields not set'),
    );
  });
});

// ---------------------------------------------------------------------------
// Metric class — predefined constants
// ---------------------------------------------------------------------------
describe('Metric predefined constants', () => {
  test('runner auto-metrics have source "runner"', () => {
    assert.equal(Metric.DURATION_MS.source, 'runner');
    assert.equal(Metric.DURATION_MS.id, 'duration_ms');
    assert.equal(Metric.EXIT_CODE.source, 'runner');
    assert.equal(Metric.EXIT_CODE.id, 'exit_code');
  });

  test('event auto-metrics have source "events" with aggregation', () => {
    assert.equal(Metric.TOKENS_IN.source, 'events');
    assert.equal(Metric.TOKENS_IN.event_type, 'model_call_end');
    assert.equal(Metric.TOKENS_IN.event_field, 'usage.tokens_in');
    assert.equal(Metric.TOKENS_IN.aggregate, 'sum');

    assert.equal(Metric.STEP_COUNT.source, 'events');
    assert.equal(Metric.STEP_COUNT.event_type, 'agent_step_start');
    assert.equal(Metric.STEP_COUNT.aggregate, 'count');

    assert.equal(Metric.TOOL_CALL_COUNT.source, 'events');
    assert.equal(Metric.TOOL_CALL_COUNT.event_type, 'tool_call_end');
    assert.equal(Metric.TOOL_CALL_COUNT.aggregate, 'count');
  });

  test('all predefined metrics default to weight 0 and primary false', () => {
    const predefined: MetricDef[] = [
      Metric.DURATION_MS, Metric.EXIT_CODE,
      Metric.TOKENS_IN, Metric.TOKENS_OUT,
      Metric.STEP_COUNT, Metric.TURN_COUNT, Metric.TOOL_CALL_COUNT,
      Metric.FILES_CREATED, Metric.FILES_MODIFIED,
      Metric.DIFF_BYTES, Metric.DIFF_LINES,
    ];
    for (const m of predefined) {
      assert.equal(m.weight, 0, `${m.id} weight`);
      assert.equal(m.primary, false, `${m.id} primary`);
    }
  });
});

// ---------------------------------------------------------------------------
// Metric.fromOutput()
// ---------------------------------------------------------------------------
describe('Metric.fromOutput()', () => {
  test('creates output metric with defaults', () => {
    const m = Metric.fromOutput('accuracy', '/metrics/accuracy');
    assert.equal(m.id, 'accuracy');
    assert.equal(m.source, 'output');
    assert.equal(m.json_pointer, '/metrics/accuracy');
    assert.equal(m.weight, 0);
    assert.equal(m.primary, false);
    assert.equal(m.direction, undefined);
  });

  test('creates output metric with all options', () => {
    const m = Metric.fromOutput('success', '/outcome', {
      weight: 1.0,
      direction: 'maximize',
      primary: true,
    });
    assert.equal(m.id, 'success');
    assert.equal(m.json_pointer, '/outcome');
    assert.equal(m.weight, 1.0);
    assert.equal(m.direction, 'maximize');
    assert.equal(m.primary, true);
  });
});

// ---------------------------------------------------------------------------
// Metric.fromEvents()
// ---------------------------------------------------------------------------
describe('Metric.fromEvents()', () => {
  test('creates event metric with count aggregate', () => {
    const m = Metric.fromEvents('error_count', {
      eventType: 'error',
      aggregate: 'count',
    });
    assert.equal(m.id, 'error_count');
    assert.equal(m.source, 'events');
    assert.equal(m.event_type, 'error');
    assert.equal(m.aggregate, 'count');
    assert.equal(m.event_field, undefined);
    assert.equal(m.weight, 0);
    assert.equal(m.primary, false);
  });

  test('creates event metric with field aggregation', () => {
    const m = Metric.fromEvents('avg_model_latency', {
      eventType: 'model_call_end',
      eventField: 'timing.duration_ms',
      aggregate: 'mean',
      direction: 'minimize',
      primary: true,
    });
    assert.equal(m.event_field, 'timing.duration_ms');
    assert.equal(m.aggregate, 'mean');
    assert.equal(m.direction, 'minimize');
    assert.equal(m.primary, true);
  });
});

// ---------------------------------------------------------------------------
// Metric artifact constants
// ---------------------------------------------------------------------------
describe('Metric artifact constants', () => {
  test('artifact auto-metrics have source "artifacts"', () => {
    assert.equal(Metric.FILES_CREATED.source, 'artifacts');
    assert.equal(Metric.FILES_CREATED.id, 'files_created');
    assert.equal(Metric.FILES_CREATED.artifact_measure, 'file_count');

    assert.equal(Metric.FILES_MODIFIED.source, 'artifacts');
    assert.equal(Metric.FILES_MODIFIED.id, 'files_modified');
    assert.equal(Metric.FILES_MODIFIED.artifact_measure, 'file_count');

    assert.equal(Metric.DIFF_BYTES.source, 'artifacts');
    assert.equal(Metric.DIFF_BYTES.id, 'diff_bytes');
    assert.equal(Metric.DIFF_BYTES.artifact_measure, 'diff_bytes');

    assert.equal(Metric.DIFF_LINES.source, 'artifacts');
    assert.equal(Metric.DIFF_LINES.id, 'diff_lines');
    assert.equal(Metric.DIFF_LINES.artifact_measure, 'diff_lines');
  });

  test('all artifact metrics default to weight 0 and primary false', () => {
    const artifactMetrics: MetricDef[] = [
      Metric.FILES_CREATED, Metric.FILES_MODIFIED,
      Metric.DIFF_BYTES, Metric.DIFF_LINES,
    ];
    for (const m of artifactMetrics) {
      assert.equal(m.weight, 0, `${m.id} weight`);
      assert.equal(m.primary, false, `${m.id} primary`);
    }
  });
});

// ---------------------------------------------------------------------------
// Metric.fromArtifacts()
// ---------------------------------------------------------------------------
describe('Metric.fromArtifacts()', () => {
  test('creates artifact metric with defaults', () => {
    const m = Metric.fromArtifacts('patch_size', { measure: 'diff_bytes' });
    assert.equal(m.id, 'patch_size');
    assert.equal(m.source, 'artifacts');
    assert.equal(m.artifact_measure, 'diff_bytes');
    assert.equal(m.artifact_glob, undefined);
    assert.equal(m.weight, 0);
    assert.equal(m.primary, false);
    assert.equal(m.direction, undefined);
  });

  test('creates artifact metric with glob filter and all options', () => {
    const m = Metric.fromArtifacts('py_files_changed', {
      measure: 'file_count',
      glob: '**/*.py',
      weight: 0.5,
      direction: 'minimize',
      primary: true,
    });
    assert.equal(m.id, 'py_files_changed');
    assert.equal(m.source, 'artifacts');
    assert.equal(m.artifact_measure, 'file_count');
    assert.equal(m.artifact_glob, '**/*.py');
    assert.equal(m.weight, 0.5);
    assert.equal(m.direction, 'minimize');
    assert.equal(m.primary, true);
  });

  test('supports total_bytes measure', () => {
    const m = Metric.fromArtifacts('output_size', { measure: 'total_bytes' });
    assert.equal(m.artifact_measure, 'total_bytes');
  });
});

// ---------------------------------------------------------------------------
// ExperimentBuilder.artifacts()
// ---------------------------------------------------------------------------
describe('ExperimentBuilder.artifacts()', () => {
  test('sets artifact collection config', () => {
    const spec = validBuilder()
      .artifacts({ collect: ['**/*.py', 'output/**'], diff: true })
      .build();
    assert.ok(spec.artifacts);
    assert.deepEqual(spec.artifacts.collect, ['**/*.py', 'output/**']);
    assert.equal(spec.artifacts.diff, true);
    assert.equal(spec.artifacts.base_dir, undefined);
  });

  test('diff defaults to false', () => {
    const spec = validBuilder()
      .artifacts({ collect: ['*.txt'] })
      .build();
    assert.ok(spec.artifacts);
    assert.equal(spec.artifacts.diff, false);
  });

  test('sets base_dir', () => {
    const spec = validBuilder()
      .artifacts({ collect: ['**/*'], diff: true, baseDir: 'workspace/src' })
      .build();
    assert.ok(spec.artifacts);
    assert.equal(spec.artifacts.base_dir, 'workspace/src');
  });

  test('copies the collect array', () => {
    const globs = ['*.py'];
    const spec = validBuilder().artifacts({ collect: globs }).build();
    globs.push('*.js');
    assert.deepEqual(spec.artifacts!.collect, ['*.py']);
  });

  test('returns this for chaining', () => {
    const builder = validBuilder();
    assert.equal(builder.artifacts({ collect: ['*'] }), builder);
  });

  test('artifacts survive build() deep copy', () => {
    const builder = validBuilder().artifacts({ collect: ['*.py'], diff: true });
    const spec1 = builder.build();
    const spec2 = builder.build();
    assert.notEqual(spec1.artifacts, spec2.artifacts);
    spec1.artifacts!.collect.push('*.js');
    assert.deepEqual(spec2.artifacts!.collect, ['*.py']);
  });

  test('spec has no artifacts section by default', () => {
    const spec = validBuilder().build();
    assert.equal(spec.artifacts, undefined);
  });
});

// ---------------------------------------------------------------------------
// ExperimentBuilder.metric()
// ---------------------------------------------------------------------------
describe('ExperimentBuilder.metric()', () => {
  test('adds metrics to spec', () => {
    const spec = validBuilder()
      .metric(Metric.DURATION_MS)
      .metric(Metric.fromOutput('success', '/outcome', { primary: true, weight: 1.0 }))
      .build();
    assert.equal(spec.metrics.length, 2);
    assert.equal(spec.metrics[0].id, 'duration_ms');
    assert.equal(spec.metrics[1].id, 'success');
    assert.equal(spec.metrics[1].weight, 1.0);
    assert.equal(spec.metrics[1].primary, true);
  });

  test('replaces metric with same id', () => {
    const spec = validBuilder()
      .metric(Metric.TOKENS_IN)
      .metric({ ...Metric.TOKENS_IN, primary: true, weight: 0.5 })
      .build();
    assert.equal(spec.metrics.length, 1);
    assert.equal(spec.metrics[0].id, 'tokens_in');
    assert.equal(spec.metrics[0].primary, true);
    assert.equal(spec.metrics[0].weight, 0.5);
  });

  test('copies the metric def (mutation-safe)', () => {
    const def = Metric.fromOutput('x', '/x');
    const spec = validBuilder().metric(def).build();
    (def as { weight: number }).weight = 999;
    assert.equal(spec.metrics[0].weight, 0);
  });

  test('returns this for chaining', () => {
    const builder = validBuilder();
    assert.equal(builder.metric(Metric.DURATION_MS), builder);
  });

  test('metrics survive build() deep copy', () => {
    const builder = validBuilder()
      .metric(Metric.fromOutput('a', '/a', { weight: 1.0 }));
    const spec1 = builder.build();
    const spec2 = builder.build();
    assert.notEqual(spec1.metrics, spec2.metrics);
    spec1.metrics[0].weight = 999;
    assert.equal(spec2.metrics[0].weight, 1.0);
  });
});

// ---------------------------------------------------------------------------
// Fluent setters
// ---------------------------------------------------------------------------
describe('ExperimentBuilder fluent API', () => {
  test('description()', () => {
    const spec = validBuilder().description('custom desc').build();
    assert.equal(spec.experiment.description, 'custom desc');
  });

  test('owner()', () => {
    const spec = validBuilder().owner('alice').build();
    assert.equal(spec.experiment.owner, 'alice');
  });

  test('tags()', () => {
    const spec = validBuilder().tags(['a', 'b']).build();
    assert.deepEqual(spec.experiment.tags, ['a', 'b']);
  });

  test('tags() copies the input array', () => {
    const input = ['x'];
    const spec = validBuilder().tags(input).build();
    input.push('y');
    assert.deepEqual(spec.experiment.tags, ['x']);
  });

  test('datasetJsonl() sets all required fields', () => {
    const spec = validBuilder()
      .datasetJsonl('my.jsonl', {
        suiteId: 'suite-2',
        splitId: 'test',
        limit: 10,
        schemaVersion: 'v2',
      })
      .build();
    assert.equal(spec.dataset.path, 'my.jsonl');
    assert.equal(spec.dataset.suite_id, 'suite-2');
    assert.equal(spec.dataset.split_id, 'test');
    assert.equal(spec.dataset.limit, 10);
    assert.equal(spec.dataset.schema_version, 'v2');
  });

  test('harnessCli() sets command and integrationLevel', () => {
    const spec = validBuilder()
      .harnessCli(['python', 'harness.py'], { integrationLevel: 'cli_events' })
      .build();
    assert.deepEqual(spec.runtime.harness.command, ['python', 'harness.py']);
    assert.equal(spec.runtime.harness.integration_level, 'cli_events');
  });

  test('harnessCli() copies the command array', () => {
    const cmd = ['python', 'h.py'];
    const spec = validBuilder()
      .harnessCli(cmd, { integrationLevel: 'cli_basic' })
      .build();
    cmd.push('--extra');
    assert.deepEqual(spec.runtime.harness.command, ['python', 'h.py']);
  });

  test('harnessCli() with custom paths', () => {
    const spec = validBuilder()
      .harnessCli(['node', 'run.js'], {
        integrationLevel: 'cli_events',
        inputPath: '/custom/in.json',
        outputPath: '/custom/out.json',
      })
      .build();
    assert.equal(spec.runtime.harness.integration_level, 'cli_events');
    assert.equal(spec.runtime.harness.input_path, '/custom/in.json');
    assert.equal(spec.runtime.harness.output_path, '/custom/out.json');
  });

  test('baseline()', () => {
    const spec = validBuilder()
      .baseline('control', { model: 'gpt-4' })
      .build();
    assert.equal(spec.baseline.variant_id, 'control');
    assert.deepEqual(spec.baseline.bindings, { model: 'gpt-4' });
  });

  test('baseline() copies bindings', () => {
    const bindings = { k: 'v' };
    const spec = validBuilder().baseline('b', bindings).build();
    bindings.k = 'mutated';
    assert.equal(spec.baseline.bindings.k, 'v');
  });

  test('addVariant() appends to variant_plan', () => {
    const spec = validBuilder()
      .addVariant('v1', { temp: 0.5 })
      .addVariant('v2', { temp: 1.0 })
      .build();
    assert.equal(spec.variant_plan.length, 2);
    assert.equal(spec.variant_plan[0].variant_id, 'v1');
    assert.deepEqual(spec.variant_plan[0].bindings, { temp: 0.5 });
    assert.equal(spec.variant_plan[1].variant_id, 'v2');
    assert.deepEqual(spec.variant_plan[1].bindings, { temp: 1.0 });
  });

  test('addVariant() copies bindings', () => {
    const bindings = { k: 1 };
    const spec = validBuilder().addVariant('v', bindings).build();
    bindings.k = 999;
    assert.equal(spec.variant_plan[0].bindings.k, 1);
  });

  test('replications()', () => {
    const spec = validBuilder().replications(5).build();
    assert.equal(spec.design.replications, 5);
  });

  test('sanitizationProfile()', () => {
    const spec = validBuilder().sanitizationProfile('custom_profile').build();
    assert.equal(spec.design.sanitization_profile, 'custom_profile');
  });

  test('randomSeed()', () => {
    const spec = validBuilder().randomSeed(42).build();
    assert.equal(spec.design.random_seed, 42);
  });

  test('maxConcurrency()', () => {
    const spec = validBuilder().maxConcurrency(4).build();
    assert.equal(spec.design.max_concurrency, 4);
  });

  test('networkMode() with allowlist', () => {
    const spec = validBuilder()
      .networkMode('allowlist_enforced', ['api.openai.com'])
      .build();
    assert.equal(spec.runtime.network.mode, 'allowlist_enforced');
    assert.deepEqual(spec.runtime.network.allowed_hosts, ['api.openai.com']);
  });

  test('networkMode() full', () => {
    const spec = validBuilder().networkMode('full').build();
    assert.equal(spec.runtime.network.mode, 'full');
    assert.deepEqual(spec.runtime.network.allowed_hosts, []);
  });

  test('sandboxImage()', () => {
    const spec = validBuilder()
      .sandboxImage('python:3.12')
      .build();
    assert.equal(spec.runtime.sandbox.mode, 'container');
    assert.equal(spec.runtime.sandbox.image, 'python:3.12');
  });

  test('localSandbox() strips container fields', () => {
    const spec = validBuilder().sandboxImage('x').localSandbox().build();
    assert.equal(spec.runtime.sandbox.mode, 'local');
    assert.equal(spec.runtime.sandbox.image, undefined);
    assert.equal(spec.runtime.sandbox.engine, undefined);
    assert.equal(spec.runtime.sandbox.hardening, undefined);
  });

  test('sandboxImage() after localSandbox() restores container mode', () => {
    const spec = validBuilder()
      .localSandbox()
      .sandboxImage('ubuntu:22.04')
      .build();
    assert.equal(spec.runtime.sandbox.mode, 'container');
    assert.equal(spec.runtime.sandbox.image, 'ubuntu:22.04');
  });
});

// ---------------------------------------------------------------------------
// Chaining
// ---------------------------------------------------------------------------
describe('ExperimentBuilder chaining', () => {
  test('all fluent methods return the same builder (this)', () => {
    const builder = validBuilder();
    assert.equal(builder.description('d'), builder);
    assert.equal(builder.owner('o'), builder);
    assert.equal(builder.tags([]), builder);
    assert.equal(builder.datasetJsonl('p', { suiteId: 's', splitId: 'd', limit: 1 }), builder);
    assert.equal(builder.harnessCli(['x'], { integrationLevel: 'cli_basic' }), builder);
    assert.equal(builder.baseline('b', {}), builder);
    assert.equal(builder.addVariant('v', {}), builder);
    assert.equal(builder.replications(1), builder);
    assert.equal(builder.sanitizationProfile('p'), builder);
    assert.equal(builder.randomSeed(1), builder);
    assert.equal(builder.maxConcurrency(1), builder);
    assert.equal(builder.metric(Metric.DURATION_MS), builder);
    assert.equal(builder.artifacts({ collect: ['*'] }), builder);
    assert.equal(builder.networkMode('none'), builder);
    assert.equal(builder.sandboxImage('x'), builder);
    assert.equal(builder.localSandbox(), builder);
  });
});

// ---------------------------------------------------------------------------
// build() immutability
// ---------------------------------------------------------------------------
describe('ExperimentBuilder build() immutability', () => {
  test('build() returns a deep copy', () => {
    const builder = validBuilder()
      .addVariant('v1', { k: 'original' });
    const spec1 = builder.build();
    const spec2 = builder.build();

    // Different object references
    assert.notEqual(spec1, spec2);
    assert.notEqual(spec1.variant_plan, spec2.variant_plan);

    // Mutating one does not affect the other
    spec1.variant_plan[0].bindings.k = 'mutated';
    assert.equal(spec2.variant_plan[0].bindings.k, 'original');
  });

  test('mutating build output does not affect builder', () => {
    const builder = validBuilder();
    const spec = builder.build();
    spec.experiment.name = 'MUTATED';
    const fresh = builder.build();
    assert.equal(fresh.experiment.name, 'My Experiment');
  });
});

// ---------------------------------------------------------------------------
// toYaml()
// ---------------------------------------------------------------------------
describe('ExperimentBuilder toYaml()', () => {
  test('produces valid YAML string', () => {
    const yaml = validBuilder().toYaml();
    assert.equal(typeof yaml, 'string');
    assert.ok(yaml.includes('version:'));
    assert.ok(yaml.includes('experiment:'));
  });

  test('YAML contains experiment id', () => {
    const yaml = validBuilder().toYaml();
    assert.ok(yaml.includes('exp-1'));
    assert.ok(yaml.includes('My Experiment'));
  });

  test('YAML contains variant plan entries', () => {
    const yaml = validBuilder()
      .addVariant('v1', { temp: 0.7 })
      .toYaml();
    assert.ok(yaml.includes('v1'));
    assert.ok(yaml.includes('0.7'));
  });

  test('YAML contains metric definitions', () => {
    const yaml = validBuilder()
      .metric(Metric.TOKENS_IN)
      .metric(Metric.fromOutput('success', '/outcome', { primary: true, weight: 1.0 }))
      .toYaml();
    assert.ok(yaml.includes('tokens_in'));
    assert.ok(yaml.includes('model_call_end'));
    assert.ok(yaml.includes('success'));
    assert.ok(yaml.includes('/outcome'));
  });

  test('YAML contains artifact config and metrics', () => {
    const yaml = validBuilder()
      .artifacts({ collect: ['**/*.py'], diff: true })
      .metric(Metric.FILES_MODIFIED)
      .metric(Metric.fromArtifacts('patch', { measure: 'diff_bytes', glob: '**/*.py' }))
      .toYaml();
    assert.ok(yaml.includes('artifacts:'));
    assert.ok(yaml.includes('**/*.py'));
    assert.ok(yaml.includes('diff: true'));
    assert.ok(yaml.includes('files_modified'));
    assert.ok(yaml.includes('diff_bytes'));
  });
});

// ---------------------------------------------------------------------------
// Complex composition
// ---------------------------------------------------------------------------
describe('ExperimentBuilder complex composition', () => {
  test('full experiment build', () => {
    const spec = ExperimentBuilder.create('swe-bench-eval', 'SWE-Bench Evaluation')
      .description('Compare models on SWE-Bench Lite')
      .owner('team-eval')
      .tags(['swe-bench', 'comparison'])
      .datasetJsonl('./swe_bench_lite.jsonl', {
        suiteId: 'swe-bench',
        splitId: 'test',
        limit: 100,
      })
      .harnessCli(['python', '-m', 'harness', 'run'], {
        integrationLevel: 'cli_events',
      })
      .baseline('gpt-4', { model: 'gpt-4', temperature: 0.0 })
      .addVariant('claude-3-opus', { model: 'claude-3-opus', temperature: 0.0 })
      .addVariant('claude-3-sonnet', { model: 'claude-3-sonnet', temperature: 0.0 })
      .replications(3)
      .sanitizationProfile('hermetic_functional_v2')
      .randomSeed(1337)
      .maxConcurrency(8)
      .metric(Metric.DURATION_MS)
      .metric(Metric.TOKENS_IN)
      .metric(Metric.TOKENS_OUT)
      .metric(Metric.fromOutput('resolved', '/metrics/resolved', {
        primary: true, weight: 1.0, direction: 'maximize',
      }))
      .metric(Metric.fromOutput('applied', '/metrics/applied', {
        primary: true, weight: 1.0, direction: 'maximize',
      }))
      .metric(Metric.fromOutput('cost_usd', '/metrics/cost_usd', { direction: 'minimize' }))
      .metric(Metric.fromOutput('duration_s', '/metrics/duration_s', { direction: 'minimize' }))
      .metric(Metric.FILES_MODIFIED)
      .metric(Metric.DIFF_LINES)
      .metric(Metric.fromArtifacts('py_patch_size', {
        measure: 'diff_bytes', glob: '**/*.py', direction: 'minimize',
      }))
      .artifacts({ collect: ['**/*.py', 'output/**'], diff: true })
      .networkMode('allowlist_enforced', ['api.openai.com', 'api.anthropic.com'])
      .sandboxImage('python:3.11-slim')
      .build();

    assert.equal(spec.experiment.id, 'swe-bench-eval');
    assert.equal(spec.experiment.description, 'Compare models on SWE-Bench Lite');
    assert.equal(spec.experiment.owner, 'team-eval');
    assert.deepEqual(spec.experiment.tags, ['swe-bench', 'comparison']);
    assert.equal(spec.dataset.path, './swe_bench_lite.jsonl');
    assert.equal(spec.dataset.suite_id, 'swe-bench');
    assert.equal(spec.dataset.limit, 100);
    assert.equal(spec.baseline.variant_id, 'gpt-4');
    assert.equal(spec.variant_plan.length, 2);
    assert.equal(spec.design.replications, 3);
    assert.equal(spec.design.max_concurrency, 8);
    assert.equal(spec.design.sanitization_profile, 'hermetic_functional_v2');
    assert.equal(spec.design.random_seed, 1337);

    // 10 metrics: duration_ms, tokens_in, tokens_out, resolved, applied, cost_usd, duration_s,
    //             files_modified, diff_lines, py_patch_size
    assert.equal(spec.metrics.length, 10);
    const primary = spec.metrics.filter((m) => m.primary);
    assert.equal(primary.length, 2);
    assert.deepEqual(primary.map((m) => m.id).sort(), ['applied', 'resolved']);

    const weighted = spec.metrics.filter((m) => m.weight > 0);
    assert.equal(weighted.length, 2);

    const artifactMetrics = spec.metrics.filter((m) => m.source === 'artifacts');
    assert.equal(artifactMetrics.length, 3);
    assert.deepEqual(artifactMetrics.map((m) => m.id).sort(), ['diff_lines', 'files_modified', 'py_patch_size']);

    // Artifacts config
    assert.ok(spec.artifacts);
    assert.deepEqual(spec.artifacts.collect, ['**/*.py', 'output/**']);
    assert.equal(spec.artifacts.diff, true);

    assert.equal(spec.runtime.network.mode, 'allowlist_enforced');
    assert.equal(spec.runtime.sandbox.image, 'python:3.11-slim');
  });
});

// ---------------------------------------------------------------------------
// Metric guardrail factories
// ---------------------------------------------------------------------------
describe('Metric guardrail factories', () => {
  test('maxTokensIn()', () => {
    const g = Metric.maxTokensIn(50000);
    assert.equal(g.metric_id, 'tokens_in');
    assert.equal(g.max, 50000);
  });

  test('maxTokensOut()', () => {
    const g = Metric.maxTokensOut(10000);
    assert.equal(g.metric_id, 'tokens_out');
    assert.equal(g.max, 10000);
  });

  test('maxDuration()', () => {
    const g = Metric.maxDuration(300000);
    assert.equal(g.metric_id, 'duration_ms');
    assert.equal(g.max, 300000);
  });

  test('maxToolCalls()', () => {
    const g = Metric.maxToolCalls(100);
    assert.equal(g.metric_id, 'tool_call_count');
    assert.equal(g.max, 100);
  });

  test('maxTurns()', () => {
    const g = Metric.maxTurns(50);
    assert.equal(g.metric_id, 'turn_count');
    assert.equal(g.max, 50);
  });

  test('maxCost()', () => {
    const g = Metric.maxCost(5.0);
    assert.equal(g.metric_id, 'cost_usd');
    assert.equal(g.max, 5.0);
  });
});

// ---------------------------------------------------------------------------
// ExperimentBuilder.guardrail()
// ---------------------------------------------------------------------------
describe('ExperimentBuilder.guardrail()', () => {
  test('adds guardrails to spec', () => {
    const spec = validBuilder()
      .guardrail(Metric.maxTokensIn(50000))
      .guardrail(Metric.maxDuration(300000))
      .build();
    assert.ok(spec.guardrails);
    assert.equal(spec.guardrails.length, 2);
    assert.equal(spec.guardrails[0].metric_id, 'tokens_in');
    assert.equal(spec.guardrails[0].max, 50000);
    assert.equal(spec.guardrails[1].metric_id, 'duration_ms');
    assert.equal(spec.guardrails[1].max, 300000);
  });

  test('replaces guardrail with same metric_id', () => {
    const spec = validBuilder()
      .guardrail(Metric.maxTokensIn(50000))
      .guardrail(Metric.maxTokensIn(100000))
      .build();
    assert.ok(spec.guardrails);
    assert.equal(spec.guardrails.length, 1);
    assert.equal(spec.guardrails[0].max, 100000);
  });

  test('copies the guardrail def (mutation-safe)', () => {
    const g = Metric.maxToolCalls(100);
    const spec = validBuilder().guardrail(g).build();
    (g as { max: number }).max = 999;
    assert.equal(spec.guardrails![0].max, 100);
  });

  test('returns this for chaining', () => {
    const builder = validBuilder();
    assert.equal(builder.guardrail(Metric.maxTurns(10)), builder);
  });

  test('spec has no guardrails section by default', () => {
    const spec = validBuilder().build();
    assert.equal(spec.guardrails, undefined);
  });

  test('guardrails survive build() deep copy', () => {
    const builder = validBuilder().guardrail(Metric.maxCost(5.0));
    const spec1 = builder.build();
    const spec2 = builder.build();
    assert.notEqual(spec1.guardrails, spec2.guardrails);
    spec1.guardrails![0].max = 999;
    assert.equal(spec2.guardrails![0].max, 5.0);
  });

  test('YAML contains guardrails section', () => {
    const yaml = validBuilder()
      .guardrail(Metric.maxTokensIn(50000))
      .guardrail(Metric.maxDuration(300000))
      .toYaml();
    assert.ok(yaml.includes('guardrails:'));
    assert.ok(yaml.includes('tokens_in'));
    assert.ok(yaml.includes('50000'));
    assert.ok(yaml.includes('duration_ms'));
    assert.ok(yaml.includes('300000'));
  });

  test('custom guardrail with metric_id only', () => {
    const g: GuardrailDef = { metric_id: 'custom_metric', max: 42 };
    const spec = validBuilder().guardrail(g).build();
    assert.equal(spec.guardrails![0].metric_id, 'custom_metric');
    assert.equal(spec.guardrails![0].max, 42);
  });
});
