import type { SchedulingPolicy, StatePolicy, ComparisonPolicy } from './experiment-builder.js';

export type JsonMap = Record<string, unknown>;

export interface LabErrorPayload {
  code: string;
  message: string;
  details?: unknown;
}

export interface LabErrorEnvelope {
  ok: false;
  error: LabErrorPayload;
}

export interface ExperimentSummary {
  experiment: string;
  workload_type: string;
  dataset: string;
  tasks: number;
  replications: number;
  variant_plan_entries: number;
  total_trials: number;
  harness: string[];
  integration_level: string;
  container_mode: boolean;
  image?: string | null;
  network: string;
  events_path?: string | null;
  tracing?: string | null;
  control_path: string;
  harness_script_resolved?: string | null;
  harness_script_exists: boolean;
  scheduling?: SchedulingPolicy;
  state_policy?: StatePolicy;
  comparison?: ComparisonPolicy;
  retry_max_attempts?: number;
}

export interface RunResult {
  run_id: string;
  run_dir: string;
}

export interface RunArtifacts {
  evidence_records_path: string;
  task_chain_states_path: string;
  benchmark_dir: string;
  benchmark_summary_path?: string | null;
}

export interface DescribeResponse {
  ok: true;
  command: 'describe';
  summary: ExperimentSummary;
}

export interface RunResponse {
  ok: true;
  command: 'run' | 'run-dev';
  summary: ExperimentSummary;
  run: RunResult;
  artifacts?: RunArtifacts;
  container?: boolean;
  executor?: 'local_docker' | 'local_process' | 'remote';
  materialize?: 'none' | 'metadata_only' | 'outputs_only' | 'full';
  remote_endpoint?: string | null;
  remote_token_env?: string | null;
  dev_setup?: string | null;
  dev_network_mode?: string;
}

export interface PublishResponse {
  ok: true;
  command: 'publish';
  bundle: string;
  run_dir: string;
}

export interface ReplayResult {
  replay_id: string;
  replay_dir: string;
  parent_trial_id: string;
  strict: boolean;
  replay_grade: string;
  harness_status: string;
}

export interface ReplayResponse {
  ok: true;
  command: 'replay';
  replay: ReplayResult;
}

export interface ForkResult {
  fork_id: string;
  fork_dir: string;
  parent_trial_id: string;
  selector: string;
  strict: boolean;
  source_checkpoint: string | null;
  fallback_mode: string;
  replay_grade: string;
  harness_status: string;
}

export interface ForkResponse {
  ok: true;
  command: 'fork';
  fork: ForkResult;
}

export interface PauseResult {
  run_id: string;
  trial_id: string;
  label: string;
  checkpoint_acked: boolean;
  stop_acked: boolean;
}

export interface PauseResponse {
  ok: true;
  command: 'pause';
  pause: PauseResult;
}

export interface ResumeResult {
  trial_id: string;
  selector: string;
  fork: ForkResult;
}

export interface ResumeResponse {
  ok: true;
  command: 'resume';
  resume: ResumeResult;
}

export interface ValidateResponse {
  ok: true;
  command: 'knobs-validate' | 'schema-validate' | 'hooks-validate';
  valid: true;
  [key: string]: unknown;
}

export type JsonCommandResponse =
  | DescribeResponse
  | RunResponse
  | ReplayResponse
  | ForkResponse
  | PauseResponse
  | ResumeResponse
  | PublishResponse
  | ValidateResponse;

export interface CommandOptions {
  cwd?: string;
  env?: NodeJS.ProcessEnv;
}

export interface LabClientOptions {
  runnerBin?: string;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
}

export interface DescribeArgs extends CommandOptions {
  experiment: string;
  overrides?: string;
}

export interface RunArgs extends DescribeArgs {
  executor?: 'local_docker' | 'local_process' | 'remote';
  materialize?: 'none' | 'metadata_only' | 'outputs_only' | 'full';
  remoteEndpoint?: string;
  remoteTokenEnv?: string;
}

export interface RunDevArgs extends DescribeArgs {
  setup?: string;
}

export interface ReplayArgs extends CommandOptions {
  runDir: string;
  trialId: string;
  strict?: boolean;
}

export interface ForkArgs extends CommandOptions {
  runDir: string;
  fromTrial: string;
  at: string;
  set?: JsonMap;
  strict?: boolean;
}

export interface PauseArgs extends CommandOptions {
  runDir: string;
  trialId?: string;
  label?: string;
  timeoutSeconds?: number;
}

export interface ResumeArgs extends CommandOptions {
  runDir: string;
  trialId?: string;
  label?: string;
  set?: JsonMap;
  strict?: boolean;
}

export interface PublishArgs extends CommandOptions {
  runDir: string;
  out?: string;
}

export interface KnobsValidateArgs extends CommandOptions {
  manifest: string;
  overrides: string;
}

export interface HooksValidateArgs extends CommandOptions {
  manifest: string;
  events: string;
}

export interface SchemaValidateArgs extends CommandOptions {
  schema: string;
  file: string;
}

// ---------------------------------------------------------------------------
// Analysis types
// ---------------------------------------------------------------------------

export interface EventCounts {
  agent_step_start: number;
  agent_step_end: number;
  model_call_end: number;
  tool_call_end: number;
  control_ack: number;
  error: number;
}

export interface VariantSummary {
  total: number;
  success_rate: number;
  primary_metric_name?: string;
  primary_metric_mean?: number;
  event_counts: EventCounts;
}

export interface AnalysisSummary {
  schema_version: string;
  baseline_id: string;
  variants: Record<string, VariantSummary>;
}

export interface ComparisonEntry {
  baseline: string;
  variant: string;
  baseline_success_rate: number;
  variant_success_rate: number;
}

export interface AnalysisComparisons {
  schema_version: string;
  comparisons: ComparisonEntry[];
}

export interface ReadAnalysisArgs extends CommandOptions {
  runDir: string;
}

export interface ReadAnalysisResponse {
  summary: AnalysisSummary;
  comparisons: AnalysisComparisons;
}

// ---------------------------------------------------------------------------
// Evidence + Benchmark artifact types
// ---------------------------------------------------------------------------

export interface CommonTrialIds {
  run_id: string;
  trial_id: string;
  variant_id: string;
  task_id: string;
  repl_idx: number;
}

export interface EvidencePolicy {
  state_policy?: 'isolate_per_trial' | 'persist_per_task' | 'accumulate';
  task_model?: 'independent' | 'dependent';
  chain_id?: string;
  chain_step_index?: number;
}

export interface EvidenceRuntime {
  executor: 'local_docker' | 'local_process' | 'remote';
  container_mode: boolean;
  exit_status: string;
  duration_ms?: number;
}

export interface EvidenceRefs {
  trial_input_ref: string;
  trial_output_ref: string;
  stdout_ref?: string;
  stderr_ref?: string;
  hook_events_ref?: string;
  harness_request_ref?: string;
  harness_response_ref?: string;
  workspace_pre_ref: string;
  workspace_post_ref: string;
  diff_incremental_ref: string;
  diff_cumulative_ref: string;
  patch_incremental_ref: string;
  patch_cumulative_ref: string;
  supplemental_refs?: string[];
}

export interface EvidencePaths {
  trial_dir?: string;
  trial_input?: string;
  trial_output?: string;
  stdout?: string;
  stderr?: string;
  hook_events?: string;
  workspace_pre_snapshot?: string;
  workspace_post_snapshot?: string;
  diff_incremental?: string;
  diff_cumulative?: string;
  patch_incremental?: string;
  patch_cumulative?: string;
}

export interface EvidenceRecord {
  schema_version: 'evidence_record_v1';
  ts?: string;
  ids: CommonTrialIds;
  policy?: EvidencePolicy;
  runtime: EvidenceRuntime;
  evidence: EvidenceRefs;
  paths?: EvidencePaths;
  ext?: JsonMap;
}

export interface TaskChainStateIds {
  trial_id: string;
  variant_id: string;
  task_id: string;
  repl_idx: number;
}

export interface TaskChainStateRecord {
  schema_version: 'task_chain_state_v1';
  ts?: string;
  run_id: string;
  chain_id: string;
  task_model: 'independent' | 'dependent';
  step_index: number;
  ids: TaskChainStateIds;
  snapshots: {
    chain_root_ref: string;
    prev_ref: string;
    post_ref: string;
  };
  diffs: {
    incremental_ref: string;
    cumulative_ref: string;
    patch_incremental_ref: string;
    patch_cumulative_ref: string;
  };
  ext?: JsonMap;
}

export interface BenchmarkIdentity {
  adapter_id: string;
  name: string;
  version?: string;
  split: string;
}

export interface BenchmarkEvaluator {
  name: string;
  version?: string;
  mode: 'official' | 'custom';
  command?: string[];
}

export interface BenchmarkAdapterManifest {
  schema_version: 'benchmark_adapter_manifest_v1';
  created_at?: string;
  adapter_id: string;
  adapter_version: string;
  benchmark: {
    name: string;
    version?: string;
    split: string;
    source?: string;
    license?: string;
  };
  execution_mode: 'predict_then_score' | 'integrated_score';
  record_schemas: {
    prediction: 'benchmark_prediction_record_v1';
    score: 'benchmark_score_record_v1';
  };
  evaluator: BenchmarkEvaluator;
  capabilities?: JsonMap;
  ext?: JsonMap;
}

export interface BenchmarkPredictionRecord {
  schema_version: 'benchmark_prediction_record_v1';
  ts?: string;
  ids: CommonTrialIds;
  benchmark: BenchmarkIdentity;
  prediction: {
    kind: 'patch' | 'text' | 'json' | 'artifact_ref';
    value?: unknown;
    artifact_ref?: string;
    metadata?: JsonMap;
  };
  metrics?: JsonMap;
  ext?: JsonMap;
}

export interface BenchmarkScoreRecord {
  schema_version: 'benchmark_score_record_v1';
  ts?: string;
  ids: CommonTrialIds;
  benchmark: BenchmarkIdentity;
  verdict: 'pass' | 'fail' | 'missing' | 'error';
  primary_metric_name: string;
  primary_metric_value: number;
  metrics?: JsonMap;
  evaluator: BenchmarkEvaluator;
  artifacts?: Array<{
    ref: string;
    logical_name?: string;
    mime_type?: string;
  }>;
  error?: {
    error_type?: string;
    message?: string;
    stack?: string;
  };
  ext?: JsonMap;
}

export interface BenchmarkSummaryVariant {
  variant_id: string;
  total: number;
  pass?: number;
  fail?: number;
  missing?: number;
  error?: number;
  pass_rate: number;
  primary_metric_name?: string;
  primary_metric_mean?: number;
}

export interface BenchmarkSummary {
  schema_version: 'benchmark_summary_v1';
  created_at?: string;
  run_id: string;
  benchmark: BenchmarkIdentity;
  evaluator?: BenchmarkEvaluator;
  totals: {
    trials: number;
    pass: number;
    fail: number;
    missing: number;
    error: number;
  };
  variants: BenchmarkSummaryVariant[];
  ext?: JsonMap;
}

export interface ReadEvidenceArgs extends CommandOptions {
  runDir: string;
}

export interface ReadEvidenceResponse {
  evidence: EvidenceRecord[];
  taskChains: TaskChainStateRecord[];
}

export interface ReadBenchmarkArgs extends CommandOptions {
  runDir: string;
}

export interface ReadBenchmarkResponse {
  manifest: BenchmarkAdapterManifest | null;
  predictions: BenchmarkPredictionRecord[];
  scores: BenchmarkScoreRecord[];
  summary: BenchmarkSummary | null;
}
