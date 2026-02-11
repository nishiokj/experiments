#!/usr/bin/env node

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const DEFAULT_DATASET = 'princeton-nlp/SWE-bench_Lite';
const DEFAULT_SPLIT = 'test';
const DEFAULT_OUTPUT = 'data/swebench_lite_curated.jsonl';
const DEFAULT_IDS = 'data/swebench_lite_curated_ids.txt';
const DEFAULT_META = 'data/swebench_lite_curated.meta.json';
const DEFAULT_COUNT = 50;
const DEFAULT_MAX_PER_REPO = 6;
const PAGE_SIZE = 100;

function toPositiveInt(raw, fallback, name) {
  if (raw === undefined) return fallback;
  const value = Number.parseInt(String(raw), 10);
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`${name} must be a positive integer; got "${raw}"`);
  }
  return value;
}

function ensureParent(filePath) {
  mkdirSync(dirname(filePath), { recursive: true });
}

function compact(text) {
  return String(text || '').replace(/\s+/g, ' ').trim();
}

function slugTaskId(instanceId) {
  return `swebench_${String(instanceId)
    .replace(/__/g, '_')
    .replace(/[^a-zA-Z0-9_]+/g, '_')
    .replace(/^_+|_+$/g, '')
    .toLowerCase()}`;
}

function loadIds(idsPath) {
  if (!existsSync(idsPath)) return [];
  return readFileSync(idsPath, 'utf8')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith('#'));
}

function saveIds(idsPath, rows) {
  ensureParent(idsPath);
  const lines = rows.map((row) => row.instance_id);
  const body = [
    '# Curated SWE-bench Lite instance IDs',
    '# One instance_id per line. Re-run generator to refresh.',
    ...lines,
    '',
  ].join('\n');
  writeFileSync(idsPath, body);
}

function deterministicCurate(rows, count, maxPerRepo) {
  const sorted = [...rows].sort((a, b) => {
    if (a.repo === b.repo) return a.instance_id.localeCompare(b.instance_id);
    return a.repo.localeCompare(b.repo);
  });

  const selected = [];
  const perRepo = new Map();
  for (const row of sorted) {
    const used = perRepo.get(row.repo) || 0;
    if (used >= maxPerRepo) continue;
    selected.push(row);
    perRepo.set(row.repo, used + 1);
    if (selected.length >= count) return selected;
  }

  const selectedIds = new Set(selected.map((row) => row.instance_id));
  for (const row of sorted) {
    if (selectedIds.has(row.instance_id)) continue;
    selected.push(row);
    if (selected.length >= count) return selected;
  }

  return selected;
}

function selectByIds(rows, ids) {
  const byId = new Map(rows.map((row) => [row.instance_id, row]));
  const selected = [];
  const missing = [];

  for (const id of ids) {
    const row = byId.get(id);
    if (!row) {
      missing.push(id);
      continue;
    }
    selected.push(row);
  }

  return { selected, missing };
}

function toTask(row) {
  const hints = compact(row.hints_text);
  const input = {
    repo: row.repo,
    instance_id: row.instance_id,
    base_commit: row.base_commit,
    prompt: compact(row.problem_statement),
  };
  if (hints.length > 0) {
    input.hints_text = hints;
  }

  return {
    task_id: slugTaskId(row.instance_id),
    source: 'swebench-lite',
    input,
    metadata: {
      created_at: row.created_at || null,
      version: row.version || null,
      environment_setup_commit: row.environment_setup_commit || null,
    },
  };
}

async function fetchPage(dataset, split, offset, length) {
  const url = new URL('https://datasets-server.huggingface.co/rows');
  url.searchParams.set('dataset', dataset);
  url.searchParams.set('config', 'default');
  url.searchParams.set('split', split);
  url.searchParams.set('offset', String(offset));
  url.searchParams.set('length', String(length));

  const res = await fetch(url);
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Failed to fetch SWE-bench rows (${res.status}): ${body.slice(0, 200)}`);
  }

  const payload = await res.json();
  const rows = Array.isArray(payload.rows) ? payload.rows : [];
  return rows
    .map((entry) => (entry && typeof entry === 'object' && entry.row ? entry.row : entry))
    .filter((row) => row && typeof row === 'object');
}

async function fetchAllRows(dataset, split) {
  let offset = 0;
  const rows = [];
  while (true) {
    const page = await fetchPage(dataset, split, offset, PAGE_SIZE);
    if (page.length === 0) break;
    rows.push(...page);
    offset += page.length;
    if (page.length < PAGE_SIZE) break;
  }
  return rows;
}

function writeJson(filePath, payload) {
  ensureParent(filePath);
  writeFileSync(filePath, JSON.stringify(payload, null, 2) + '\n');
}

async function main() {
  const { values } = parseArgs({
    options: {
      dataset: { type: 'string', default: DEFAULT_DATASET },
      split: { type: 'string', default: DEFAULT_SPLIT },
      output: { type: 'string', default: DEFAULT_OUTPUT },
      ids: { type: 'string', default: DEFAULT_IDS },
      meta: { type: 'string', default: DEFAULT_META },
      count: { type: 'string', default: String(DEFAULT_COUNT) },
      'max-per-repo': { type: 'string', default: String(DEFAULT_MAX_PER_REPO) },
      'refresh-ids': { type: 'boolean', default: false },
      'dry-run': { type: 'boolean', default: false },
    },
    allowPositionals: false,
  });

  const dataset = values.dataset;
  const split = values.split;
  const count = toPositiveInt(values.count, DEFAULT_COUNT, '--count');
  const maxPerRepo = toPositiveInt(values['max-per-repo'], DEFAULT_MAX_PER_REPO, '--max-per-repo');
  const outputPath = resolve(values.output);
  const idsPath = resolve(values.ids);
  const metaPath = resolve(values.meta);
  const refreshIds = values['refresh-ids'];
  const dryRun = values['dry-run'];

  console.log(`Fetching ${dataset} (${split})...`);
  const rows = await fetchAllRows(dataset, split);
  if (rows.length === 0) {
    throw new Error('No rows returned from SWE-bench Lite source.');
  }

  let selectedRows = [];
  let selectedFrom = 'existing_ids';
  const existingIds = refreshIds ? [] : loadIds(idsPath);

  if (existingIds.length > 0) {
    const { selected, missing } = selectByIds(rows, existingIds);
    if (missing.length > 0) {
      throw new Error(
        `Curated ID file references unknown instances (${missing.length} missing). ` +
        `First missing: ${missing.slice(0, 5).join(', ')}`,
      );
    }
    selectedRows = selected.slice(0, count);
  } else {
    selectedFrom = 'deterministic_repo_balanced';
    selectedRows = deterministicCurate(rows, count, maxPerRepo);
    if (!dryRun) {
      saveIds(idsPath, selectedRows);
    }
  }

  if (selectedRows.length === 0) {
    throw new Error('Selection produced 0 rows. Increase --count or relax curation settings.');
  }

  const tasks = selectedRows.slice(0, count).map(toTask);
  const jsonl = tasks.map((task) => JSON.stringify(task)).join('\n') + '\n';

  const repoCounts = {};
  for (const row of selectedRows.slice(0, count)) {
    repoCounts[row.repo] = (repoCounts[row.repo] || 0) + 1;
  }

  const summary = {
    dataset,
    split,
    total_source_rows: rows.length,
    selected_rows: tasks.length,
    selected_from: selectedFrom,
    output: values.output,
    ids_file: values.ids,
    generated_at: new Date().toISOString(),
    repo_distribution: repoCounts,
  };

  if (!dryRun) {
    ensureParent(outputPath);
    writeFileSync(outputPath, jsonl);
    writeJson(metaPath, summary);
  }

  console.log(`Prepared ${tasks.length} tasks -> ${values.output}`);
  console.log(`Curation IDs: ${values.ids} (${selectedFrom})`);
  console.log(`Meta summary: ${values.meta}`);
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : String(err));
  process.exit(1);
});
