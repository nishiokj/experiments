import type { HookEvent } from './hook-events.js';
import type { TrialOutput } from './trial-output.js';

export type JsonValue =
  | string
  | number
  | boolean
  | null
  | JsonValue[]
  | { [key: string]: JsonValue };

// ---------------------------------------------------------------------------
// Standardized runner contracts (v1)
// ---------------------------------------------------------------------------

export interface WorkspaceContractV1 {
  root: '/workspace';
  task_manifest_path: '/workspace/.agentlab/task-manifest.json';
  artifacts_dir: '/workspace/.agentlab/artifacts';
}

export interface InvocationEnvContractV1 {
  trial_input: 'AGENTLAB_TRIAL_INPUT';
  trial_output: 'AGENTLAB_TRIAL_OUTPUT';
  control_path: 'AGENTLAB_CONTROL_PATH';
  harness_root: 'AGENTLAB_HARNESS_ROOT';
}

export interface InvocationContractV1 {
  command: string[];
  env: InvocationEnvContractV1;
}

export interface MountSemanticsContractV1 {
  dataset_pack_ref_format: 'sha256:<hex64>';
  read_only: true;
}

export interface EventOutputContractV1 {
  run_events_jsonl: '/state/harness_events.jsonl';
  result_summary: '/out/trial_output.json';
}

export interface RunnerBoundaryManifestV1 {
  schema_version: 'runner_boundary_manifest_v1';
  workspace: WorkspaceContractV1;
  mount_semantics: MountSemanticsContractV1;
  invocation: InvocationContractV1;
  event_output: EventOutputContractV1;
}

export const WORKSPACE_CONTRACT_V1: WorkspaceContractV1 = {
  root: '/workspace',
  task_manifest_path: '/workspace/.agentlab/task-manifest.json',
  artifacts_dir: '/workspace/.agentlab/artifacts',
};

export const INVOCATION_ENV_CONTRACT_V1: InvocationEnvContractV1 = {
  trial_input: 'AGENTLAB_TRIAL_INPUT',
  trial_output: 'AGENTLAB_TRIAL_OUTPUT',
  control_path: 'AGENTLAB_CONTROL_PATH',
  harness_root: 'AGENTLAB_HARNESS_ROOT',
};

export const EVENT_OUTPUT_CONTRACT_V1: EventOutputContractV1 = {
  run_events_jsonl: '/state/harness_events.jsonl',
  result_summary: '/out/trial_output.json',
};

const MOUNT_SEMANTICS_CONTRACT_V1: MountSemanticsContractV1 = {
  dataset_pack_ref_format: 'sha256:<hex64>',
  read_only: true,
};

export function createRunnerBoundaryManifest(
  command: readonly string[],
): RunnerBoundaryManifestV1 {
  if (command.length === 0) {
    throw new Error('invocation command must have at least one token');
  }
  for (const token of command) {
    if (!token.trim()) {
      throw new Error('invocation command tokens must be non-empty');
    }
  }
  return {
    schema_version: 'runner_boundary_manifest_v1',
    workspace: { ...WORKSPACE_CONTRACT_V1 },
    mount_semantics: { ...MOUNT_SEMANTICS_CONTRACT_V1 },
    invocation: {
      command: [...command],
      env: { ...INVOCATION_ENV_CONTRACT_V1 },
    },
    event_output: { ...EVENT_OUTPUT_CONTRACT_V1 },
  };
}

// ---------------------------------------------------------------------------
// Input boundary (user-implemented mapper target)
// All benchmark inputs must compile into:
//   task + workspace_files + mount_references + limits
// ---------------------------------------------------------------------------

export interface WorkspaceFileV1 {
  path: string;
  content: string;
  encoding?: 'utf8' | 'base64';
  executable?: boolean;
}

export interface MountReferenceV1 {
  dataset_pack_ref: string;
  mount_path: string;
  read_only: true;
}

export interface TaskLimitsV1 {
  max_steps?: number;
  max_total_tokens?: number;
  max_tool_calls?: number;
  trial_seconds?: number;
}

export interface TaskBoundaryV1 {
  schema_version: 'task_boundary_v1';
  task: Record<string, JsonValue>;
  workspace_files: WorkspaceFileV1[];
  mount_references: MountReferenceV1[];
  limits: TaskLimitsV1;
}

export interface InputMapperContext {
  index: number;
}

export interface InputMapper<TInput> {
  map(input: TInput, context: InputMapperContext): TaskBoundaryV1;
}

const TASK_BOUNDARY_KEYS = new Set([
  'schema_version',
  'task',
  'workspace_files',
  'mount_references',
  'limits',
]);

const DATASET_PACK_REF_RE = /^sha256:[0-9a-f]{64}$/;

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object' && !Array.isArray(value);
}

function isJsonValue(value: unknown): value is JsonValue {
  if (
    value === null ||
    typeof value === 'string' ||
    typeof value === 'number' ||
    typeof value === 'boolean'
  ) {
    return true;
  }
  if (Array.isArray(value)) {
    return value.every((item) => isJsonValue(item));
  }
  if (isPlainObject(value)) {
    return Object.values(value).every((item) => isJsonValue(item));
  }
  return false;
}

function assertPositiveInt(
  value: number | undefined,
  fieldName: keyof TaskLimitsV1,
): void {
  if (value === undefined) {
    return;
  }
  if (!Number.isInteger(value) || value <= 0) {
    throw new Error(`${fieldName} must be a positive integer when provided`);
  }
}

function readOptionalNumber(
  obj: Record<string, unknown>,
  fieldName: keyof TaskLimitsV1,
): number | undefined {
  const value = obj[fieldName];
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== 'number') {
    throw new Error(`${fieldName} must be a number when provided`);
  }
  return value;
}

export function assertTaskBoundaryV1(boundary: unknown): asserts boundary is TaskBoundaryV1 {
  if (!isPlainObject(boundary)) {
    throw new Error('task boundary must be an object');
  }

  const keys = Object.keys(boundary);
  for (const key of keys) {
    if (!TASK_BOUNDARY_KEYS.has(key)) {
      throw new Error(
        `task boundary contains unsupported key "${key}". ` +
          'Boundary must compile into exactly: task + workspace_files + mount_references + limits',
      );
    }
  }

  if (boundary.schema_version !== 'task_boundary_v1') {
    throw new Error('task boundary schema_version must be "task_boundary_v1"');
  }

  if (!isPlainObject(boundary.task)) {
    throw new Error('task boundary task must be an object');
  }
  for (const [key, value] of Object.entries(boundary.task)) {
    if (!isJsonValue(value)) {
      throw new Error(`task field "${key}" is not valid JSON`);
    }
  }

  if (!Array.isArray(boundary.workspace_files)) {
    throw new Error('task boundary workspace_files must be an array');
  }
  for (const [index, file] of boundary.workspace_files.entries()) {
    if (!isPlainObject(file)) {
      throw new Error(`workspace_files[${index}] must be an object`);
    }
    if (typeof file.path !== 'string' || !file.path.trim()) {
      throw new Error(`workspace_files[${index}].path must be a non-empty string`);
    }
    if (file.path.startsWith('/')) {
      throw new Error(`workspace_files[${index}].path must be relative to /workspace`);
    }
    if (typeof file.content !== 'string') {
      throw new Error(`workspace_files[${index}].content must be a string`);
    }
    if (
      file.encoding !== undefined &&
      file.encoding !== 'utf8' &&
      file.encoding !== 'base64'
    ) {
      throw new Error(`workspace_files[${index}].encoding must be "utf8" or "base64"`);
    }
    if (file.executable !== undefined && typeof file.executable !== 'boolean') {
      throw new Error(`workspace_files[${index}].executable must be a boolean when provided`);
    }
  }

  if (!Array.isArray(boundary.mount_references)) {
    throw new Error('task boundary mount_references must be an array');
  }
  for (const [index, mount] of boundary.mount_references.entries()) {
    if (!isPlainObject(mount)) {
      throw new Error(`mount_references[${index}] must be an object`);
    }
    if (mount.read_only !== true) {
      throw new Error(`mount_references[${index}].read_only must be true`);
    }
    if (typeof mount.mount_path !== 'string' || !mount.mount_path.trim()) {
      throw new Error(`mount_references[${index}].mount_path must be a non-empty string`);
    }
    if (!mount.mount_path.startsWith('/workspace')) {
      throw new Error(`mount_references[${index}].mount_path must target /workspace`);
    }
    if (
      typeof mount.dataset_pack_ref !== 'string' ||
      !DATASET_PACK_REF_RE.test(mount.dataset_pack_ref)
    ) {
      throw new Error(
        `mount_references[${index}].dataset_pack_ref must match sha256:<hex64>`,
      );
    }
  }

  if (!isPlainObject(boundary.limits)) {
    throw new Error('task boundary limits must be an object');
  }
  const limits = boundary.limits;
  assertPositiveInt(readOptionalNumber(limits, 'max_steps'), 'max_steps');
  assertPositiveInt(
    readOptionalNumber(limits, 'max_total_tokens'),
    'max_total_tokens',
  );
  assertPositiveInt(readOptionalNumber(limits, 'max_tool_calls'), 'max_tool_calls');
  assertPositiveInt(readOptionalNumber(limits, 'trial_seconds'), 'trial_seconds');
}

export function compileTaskBoundaries<TInput>(
  inputs: readonly TInput[],
  mapper: InputMapper<TInput>,
): TaskBoundaryV1[] {
  return inputs.map((input, index) => {
    const boundary = mapper.map(input, { index });
    assertTaskBoundaryV1(boundary);
    return boundary;
  });
}

export function taskBoundariesToJsonl(boundaries: readonly TaskBoundaryV1[]): string {
  const lines = boundaries.map((boundary) => {
    assertTaskBoundaryV1(boundary);
    return JSON.stringify(boundary);
  });
  if (lines.length === 0) {
    return '';
  }
  return `${lines.join('\n')}\n`;
}

// ---------------------------------------------------------------------------
// Outcome boundary (runner-emitted boundary -> user mapper)
// ---------------------------------------------------------------------------

export interface OutcomeResultSummaryV1 {
  ids: TrialOutput['ids'];
  outcome: TrialOutput['outcome'];
  metrics?: TrialOutput['metrics'];
  objective?: TrialOutput['objective'];
  artifacts?: TrialOutput['artifacts'];
  checkpoints?: TrialOutput['checkpoints'];
  error?: TrialOutput['error'];
  ext?: TrialOutput['ext'];
}

export interface OutcomeBoundaryV1 {
  schema_version: 'outcome_boundary_v1';
  run_events: HookEvent[];
  result_summary: OutcomeResultSummaryV1;
}

export interface OutcomeMapper<TMappedOutcome> {
  map(boundary: OutcomeBoundaryV1): TMappedOutcome | Promise<TMappedOutcome>;
}

export function createOutcomeBoundary(
  trialOutput: TrialOutput,
  runEvents: readonly HookEvent[] = [],
): OutcomeBoundaryV1 {
  return {
    schema_version: 'outcome_boundary_v1',
    run_events: [...runEvents],
    result_summary: {
      ids: trialOutput.ids,
      outcome: trialOutput.outcome,
      metrics: trialOutput.metrics,
      objective: trialOutput.objective,
      artifacts: trialOutput.artifacts,
      checkpoints: trialOutput.checkpoints,
      error: trialOutput.error,
      ext: trialOutput.ext,
    },
  };
}

export async function mapOutcome<TMappedOutcome>(
  boundary: OutcomeBoundaryV1,
  mapper: OutcomeMapper<TMappedOutcome>,
): Promise<TMappedOutcome> {
  return mapper.map(boundary);
}
