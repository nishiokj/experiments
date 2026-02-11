// ---------------------------------------------------------------------------
// Canonicalized Trial Output Types
// Mirrors trial_output_v1.jsonschema
// ---------------------------------------------------------------------------

export type TrialOutcome = 'success' | 'failure' | 'missing' | 'error';

export interface TrialIds {
  run_id: string;
  trial_id: string;
  variant_id: string;
  task_id: string;
  repl_idx: number;
}

export interface TrialError {
  error_type?: string;
  message?: string;
  stack?: string;
}

export interface ArtifactDecl {
  path: string;
  logical_name?: string;
  mime_type?: string;
}

export interface CheckpointDecl {
  path: string;
  logical_name?: string;
  step?: number;
  epoch?: number;
}

export interface ObjectiveDef {
  name: string;
  value: number;
  direction?: 'maximize' | 'minimize';
}

export interface TrialOutput {
  schema_version: 'trial_output_v1';
  ids: TrialIds;
  outcome: TrialOutcome;
  answer?: string | Record<string, unknown> | unknown[];
  metrics?: Record<string, number | string | boolean | null>;
  objective?: ObjectiveDef;
  artifacts?: ArtifactDecl[];
  checkpoints?: CheckpointDecl[];
  error?: TrialError;
  ext?: Record<string, unknown>;
}
