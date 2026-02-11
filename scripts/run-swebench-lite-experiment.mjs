#!/usr/bin/env node

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { parseArgs } from 'node:util';

function loadJsonEnv(name, fallback) {
  const raw = process.env[name];
  if (!raw) return fallback;
  try {
    return JSON.parse(raw);
  } catch (err) {
    throw new Error(`Invalid JSON in ${name}: ${err instanceof Error ? err.message : String(err)}`);
  }
}

function loadHarnessCommand() {
  const fromJson = loadJsonEnv('AGENTLAB_HARNESS_CMD_JSON', null);
  if (Array.isArray(fromJson) && fromJson.length > 0 && fromJson.every((v) => typeof v === 'string')) {
    return fromJson;
  }

  const fromShell = process.env.AGENTLAB_HARNESS_CMD;
  if (fromShell && fromShell.trim().length > 0) {
    // Keep parsing simple: use JSON env for quoted args.
    return fromShell.trim().split(/\s+/);
  }

  return ['python', './harness.py', 'run'];
}

function loadPositiveInt(raw, fallback, label) {
  if (raw === undefined || raw === null || raw === '') return fallback;
  const parsed = Number.parseInt(String(raw), 10);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    throw new Error(`${label} must be a positive integer; got "${raw}"`);
  }
  return parsed;
}

function countJsonlRecords(path) {
  const text = readFileSync(path, 'utf8');
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0).length;
}

function ensureWorkloadType(yamlText) {
  const lines = yamlText.split('\n');
  const expIdx = lines.findIndex((line) => line.trim() === 'experiment:');
  if (expIdx < 0) return yamlText;

  let blockEnd = lines.length;
  for (let i = expIdx + 1; i < lines.length; i += 1) {
    const line = lines[i];
    if (line.trim().length === 0) continue;
    if (!line.startsWith('  ')) {
      blockEnd = i;
      break;
    }
  }

  const hasWorkload = lines
    .slice(expIdx + 1, blockEnd)
    .some((line) => line.startsWith('  workload_type:'));
  if (hasWorkload) {
    return yamlText;
  }

  lines.splice(expIdx + 1, 0, '  workload_type: agent_harness');
  return lines.join('\n');
}

async function loadSdk() {
  try {
    return await import('@agentlab/sdk');
  } catch (_) {
    const fallback = resolve(process.cwd(), 'sdk/dist/src/index.js');
    if (!existsSync(fallback)) {
      throw new Error(
        'SDK import failed. Build local SDK first: cd sdk && npm install && npm run build',
      );
    }
    return import(fallback);
  }
}

async function main() {
  const { values } = parseArgs({
    options: {
      dataset: { type: 'string', default: 'data/swebench_lite_curated.jsonl' },
      experiment: { type: 'string', default: '.lab/experiments/swebench_lite_curated.yaml' },
      'write-only': { type: 'boolean', default: false },
      'describe-only': { type: 'boolean', default: false },
      limit: { type: 'string' },
      replications: { type: 'string', default: process.env.AGENTLAB_REPLICATIONS || '1' },
      seed: { type: 'string', default: process.env.AGENTLAB_RANDOM_SEED || '42' },
      concurrency: { type: 'string', default: process.env.AGENTLAB_MAX_CONCURRENCY || '1' },
      'integration-level': {
        type: 'string',
        default: process.env.AGENTLAB_INTEGRATION_LEVEL || 'cli_events',
      },
      'container-image': {
        type: 'string',
        default: process.env.AGENTLAB_SANDBOX_IMAGE || 'python:3.11-slim',
      },
      'runner-bin': { type: 'string' },
    },
    allowPositionals: false,
  });

  const cwd = process.cwd();
  const datasetAbs = resolve(cwd, values.dataset);
  if (!existsSync(datasetAbs)) {
    throw new Error(
      `Dataset not found at ${values.dataset}. Generate it first with:\n` +
      `  node scripts/build-curated-swebench-lite.mjs`,
    );
  }

  const expAbs = resolve(cwd, values.experiment);
  mkdirSync(dirname(expAbs), { recursive: true });

  const replications = loadPositiveInt(values.replications, 1, '--replications');
  const randomSeed = loadPositiveInt(values.seed, 42, '--seed');
  const maxConcurrency = loadPositiveInt(values.concurrency, 1, '--concurrency');
  const datasetCount = countJsonlRecords(datasetAbs);
  const limit = loadPositiveInt(values.limit, datasetCount, '--limit');
  const safeLimit = Math.min(limit, datasetCount);
  if (safeLimit <= 0) {
    throw new Error('Dataset is empty.');
  }

  const harnessCommand = loadHarnessCommand();
  const integrationLevel = values['integration-level'];
  const image = values['container-image'];

  const expDirAbs = dirname(expAbs);
  const datasetRelFromExp = relative(expDirAbs, datasetAbs);

  const baselineBindings = loadJsonEnv('AGENTLAB_BASELINE_BINDINGS_JSON', {
    prompt_profile: 'baseline',
  });
  const treatmentBindings = loadJsonEnv('AGENTLAB_TREATMENT_BINDINGS_JSON', {
    prompt_profile: 'treatment',
  });

  const { ExperimentBuilder, LabClient, Metric } = await loadSdk();

  const builder = ExperimentBuilder.create(
    'swebench_lite_curated_actual_harness',
    'SWE-bench Lite Curated (Actual Harness)',
  )
    .description('Strict containerized eval over curated SWE-bench Lite with the real harness.')
    .owner('jevinnishioka')
    .tags(['swebench-lite', 'curated', 'container', 'strict'])
    .datasetJsonl(datasetRelFromExp, {
      suiteId: 'swebench_lite_curated',
      splitId: 'test',
      limit: safeLimit,
    })
    .harnessCli(harnessCommand, { integrationLevel })
    .sanitizationProfile('hermetic_functional_v2')
    .replications(replications)
    .randomSeed(randomSeed)
    .maxConcurrency(maxConcurrency)
    .baseline('control', baselineBindings)
    .addVariant('treatment', treatmentBindings)
    .metric(Metric.DURATION_MS)
    .metric(Metric.TOKENS_IN)
    .metric(Metric.TOKENS_OUT)
    .metric(Metric.TURN_COUNT)
    .metric(Metric.fromOutput('success', '/outcome', {
      primary: true,
      weight: 1.0,
      direction: 'maximize',
    }))
    .metric(Metric.fromOutput('latency_ms', '/metrics/latency_ms', {
      primary: false,
      weight: 0,
      direction: 'minimize',
    }))
    .artifacts({
      collect: ['artifacts/**', 'output/**', '**/*.patch'],
      diff: true,
    })
    .metric(Metric.FILES_MODIFIED)
    .metric(Metric.DIFF_LINES)
    .networkMode('none')
    .sandboxImage(image);

  const yaml = ensureWorkloadType(builder.toYaml());
  writeFileSync(expAbs, yaml);
  console.log(`Wrote experiment config: ${values.experiment}`);
  console.log(`Harness command: ${JSON.stringify(harnessCommand)}`);
  console.log(`Dataset tasks: ${datasetCount} (limit=${safeLimit})`);
  console.log(`Container image: ${image}`);

  if (values['write-only']) {
    console.log('Write only; skipping describe and run.');
    return;
  }

  const clientOptions = values['runner-bin']
    ? { cwd, runnerBin: values['runner-bin'] }
    : { cwd };
  const client = new LabClient(clientOptions);
  const describe = await client.describe({ experiment: values.experiment });
  console.log(`Planned trials: ${describe.summary.total_trials}`);

  if (values['describe-only']) {
    console.log('Describe only; skipping run.');
    return;
  }

  const run = await client.run({ experiment: values.experiment });
  console.log(`Run complete: ${run.run.run_id}`);
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
});
