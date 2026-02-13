import { stringify as yamlStringify } from 'yaml';

export type Bindings = Record<string, unknown>;

export interface DatasetJsonlOptions {
  suiteId: string;
  provider?: 'local_jsonl';
  schemaVersion?: string;
  splitId: string;
  limit: number;
}

export interface HarnessCliOptions {
  integrationLevel: 'cli_basic' | 'cli_events' | 'otel' | 'sdk_control' | 'sdk_full';
  inputPath?: string;
  outputPath?: string;
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

export type MetricSource = 'runner' | 'events' | 'output' | 'artifacts';
export type ArtifactMeasure = 'file_count' | 'diff_bytes' | 'diff_lines' | 'total_bytes';
export type MetricAggregate = 'sum' | 'count' | 'max' | 'min' | 'mean' | 'last';

// ---------------------------------------------------------------------------
// Guardrails
// ---------------------------------------------------------------------------

export interface GuardrailDef {
  metric_id: string;
  max?: number;
}

export interface MetricDef {
  id: string;
  source: MetricSource;
  /** For source: 'output' — JSON pointer into trial_output.json */
  json_pointer?: string;
  /** For source: 'events' — which hook event type to aggregate */
  event_type?: string;
  /** For source: 'events' — dot-path to the numeric field within the event */
  event_field?: string;
  /** For source: 'events' — how to aggregate across events in a trial */
  aggregate?: MetricAggregate;
  /** For source: 'artifacts' — what to measure from collected artifacts */
  artifact_measure?: ArtifactMeasure;
  /** For source: 'artifacts' — optional glob filter for the measurement */
  artifact_glob?: string;
  /** 0 = observe only (default). > 0 = contributes to composite score. */
  weight: number;
  /** Whether higher or lower is better. */
  direction?: 'maximize' | 'minimize';
  /** Primary metrics are highlighted in analysis summaries. */
  primary: boolean;
}

/**
 * Factory for metric definitions. Predefined constants for runner/event
 * auto-metrics, plus helpers for output-derived and custom event metrics.
 */
export class Metric {
  // -- Runner auto-metrics (always tracked, no harness involvement) ----------

  static readonly DURATION_MS: MetricDef = {
    id: 'duration_ms', source: 'runner', weight: 0, primary: false,
  };
  static readonly EXIT_CODE: MetricDef = {
    id: 'exit_code', source: 'runner', weight: 0, primary: false,
  };

  // -- Event auto-metrics (tracked when integrationLevel >= cli_events) ------

  static readonly TOKENS_IN: MetricDef = {
    id: 'tokens_in', source: 'events',
    event_type: 'model_call_end', event_field: 'usage.tokens_in', aggregate: 'sum',
    weight: 0, primary: false,
  };
  static readonly TOKENS_OUT: MetricDef = {
    id: 'tokens_out', source: 'events',
    event_type: 'model_call_end', event_field: 'usage.tokens_out', aggregate: 'sum',
    weight: 0, primary: false,
  };
  static readonly STEP_COUNT: MetricDef = {
    id: 'step_count', source: 'events',
    event_type: 'agent_step_start', aggregate: 'count',
    weight: 0, primary: false,
  };
  static readonly TURN_COUNT: MetricDef = {
    id: 'turn_count', source: 'events',
    event_type: 'model_call_end', aggregate: 'count',
    weight: 0, primary: false,
  };
  static readonly TOOL_CALL_COUNT: MetricDef = {
    id: 'tool_call_count', source: 'events',
    event_type: 'tool_call_end', aggregate: 'count',
    weight: 0, primary: false,
  };

  // -- Artifact auto-metrics (tracked when artifacts.diff is enabled) --------

  static readonly FILES_CREATED: MetricDef = {
    id: 'files_created', source: 'artifacts',
    artifact_measure: 'file_count', weight: 0, primary: false,
  };
  static readonly FILES_MODIFIED: MetricDef = {
    id: 'files_modified', source: 'artifacts',
    artifact_measure: 'file_count', weight: 0, primary: false,
  };
  static readonly DIFF_BYTES: MetricDef = {
    id: 'diff_bytes', source: 'artifacts',
    artifact_measure: 'diff_bytes', weight: 0, primary: false,
  };
  static readonly DIFF_LINES: MetricDef = {
    id: 'diff_lines', source: 'artifacts',
    artifact_measure: 'diff_lines', weight: 0, primary: false,
  };

  // -- Factories -------------------------------------------------------------

  /** Metric extracted from a field in trial_output.json. */
  static fromOutput(id: string, jsonPointer: string, options?: {
    weight?: number;
    direction?: 'maximize' | 'minimize';
    primary?: boolean;
  }): MetricDef {
    return {
      id,
      source: 'output',
      json_pointer: jsonPointer,
      weight: options?.weight ?? 0,
      direction: options?.direction,
      primary: options?.primary ?? false,
    };
  }

  /** Metric computed by aggregating a field across hook events in a trial. */
  static fromEvents(id: string, options: {
    eventType: string;
    eventField?: string;
    aggregate: MetricAggregate;
    weight?: number;
    direction?: 'maximize' | 'minimize';
    primary?: boolean;
  }): MetricDef {
    return {
      id,
      source: 'events',
      event_type: options.eventType,
      event_field: options.eventField,
      aggregate: options.aggregate,
      weight: options?.weight ?? 0,
      direction: options?.direction,
      primary: options?.primary ?? false,
    };
  }

  /** Metric computed from workspace artifacts collected after a trial. */
  static fromArtifacts(id: string, options: {
    measure: ArtifactMeasure;
    glob?: string;
    weight?: number;
    direction?: 'maximize' | 'minimize';
    primary?: boolean;
  }): MetricDef {
    return {
      id,
      source: 'artifacts',
      artifact_measure: options.measure,
      artifact_glob: options.glob,
      weight: options?.weight ?? 0,
      direction: options?.direction,
      primary: options?.primary ?? false,
    };
  }

  // -- Guardrail factories ---------------------------------------------------

  static maxTokensIn(n: number): GuardrailDef {
    return { metric_id: 'tokens_in', max: n };
  }

  static maxTokensOut(n: number): GuardrailDef {
    return { metric_id: 'tokens_out', max: n };
  }

  static maxDuration(ms: number): GuardrailDef {
    return { metric_id: 'duration_ms', max: ms };
  }

  static maxToolCalls(n: number): GuardrailDef {
    return { metric_id: 'tool_call_count', max: n };
  }

  static maxTurns(n: number): GuardrailDef {
    return { metric_id: 'turn_count', max: n };
  }

  static maxCost(n: number): GuardrailDef {
    return { metric_id: 'cost_usd', max: n };
  }

  private constructor() {} // no instances
}

// ---------------------------------------------------------------------------
// Design Policies
// ---------------------------------------------------------------------------

export type SchedulingPolicy = 'paired_interleaved' | 'variant_sequential' | 'randomized';
export type StatePolicy = 'isolate_per_trial' | 'persist_per_task' | 'accumulate';
export type ComparisonPolicy = 'paired' | 'unpaired' | 'none';

export type RetryTrigger = 'error' | 'timeout' | 'failure';

export interface RetryPolicy {
  max_attempts: number;
  retry_on?: readonly RetryTrigger[];
}

export interface PruningPolicy {
  max_consecutive_failures?: number;
}

export interface DesignPolicies {
  scheduling: SchedulingPolicy;
  state: StatePolicy;
  comparison: ComparisonPolicy;
  retry: RetryPolicy;
  pruning?: PruningPolicy;
}

export type BenchmarkTaskModel = 'independent' | 'dependent';
export type BenchmarkScoringLifecycle = 'predict_then_score' | 'integrated_score';

export interface BenchmarkTypePolicy {
  task_model?: BenchmarkTaskModel;
  reset_strategy?: 'per_trial' | 'per_chain' | 'never';
  evaluator_mode?: 'official' | 'custom';
  scoring_lifecycle?: BenchmarkScoringLifecycle;
  required_evidence_classes?: string[];
  chain_failure_policy?: 'stop_on_error' | 'continue_with_flag';
}

export interface BenchmarkAdapterConfig {
  command: string[];
  manifest?: Record<string, unknown>;
}

export interface BenchmarkConfig {
  policy?: BenchmarkTypePolicy;
  adapter?: BenchmarkAdapterConfig;
}

function copyPolicies(p: DesignPolicies): DesignPolicies {
  return {
    scheduling: p.scheduling,
    state: p.state,
    comparison: p.comparison,
    retry: {
      max_attempts: p.retry.max_attempts,
      retry_on: p.retry.retry_on ? [...p.retry.retry_on] : undefined,
    },
    pruning: p.pruning ? { ...p.pruning } : undefined,
  };
}

// ---------------------------------------------------------------------------
// Experiment Type Presets
// ---------------------------------------------------------------------------

const DEFAULT_RETRY: RetryPolicy = { max_attempts: 1 };

export const ExperimentType = {
  AB_TEST: {
    scheduling: 'paired_interleaved',
    state: 'isolate_per_trial',
    comparison: 'paired',
    retry: { ...DEFAULT_RETRY },
  } satisfies DesignPolicies,

  MULTI_VARIANT: {
    scheduling: 'paired_interleaved',
    state: 'isolate_per_trial',
    comparison: 'paired',
    retry: { ...DEFAULT_RETRY },
  } satisfies DesignPolicies,

  PARAMETER_SWEEP: {
    scheduling: 'variant_sequential',
    state: 'isolate_per_trial',
    comparison: 'unpaired',
    retry: { ...DEFAULT_RETRY },
  } satisfies DesignPolicies,

  REGRESSION: {
    scheduling: 'variant_sequential',
    state: 'isolate_per_trial',
    comparison: 'none',
    retry: { max_attempts: 3, retry_on: ['error'] },
  } satisfies DesignPolicies,
} as const;

// ---------------------------------------------------------------------------
// ExperimentSpec
// ---------------------------------------------------------------------------

export interface ExperimentSpec {
  version: '0.3';
  experiment: {
    id: string;
    name: string;
    description?: string;
    owner?: string;
    tags?: string[];
  };
  dataset: {
    suite_id: string;
    provider: 'local_jsonl';
    path: string;
    schema_version: string;
    split_id: string;
    limit: number;
  };
  design: {
    sanitization_profile: string;
    comparison: ComparisonPolicy;
    replications: number;
    random_seed: number;
    shuffle_tasks: boolean;
    max_concurrency: number;
    policies?: DesignPolicies;
  };
  metrics: MetricDef[];
  guardrails?: GuardrailDef[];
  artifacts?: {
    /** Glob patterns for files to collect from workspace post-trial */
    collect: string[];
    /** Compute workspace diff (pre vs post trial snapshot) */
    diff: boolean;
    /** Base directory for collection, relative to workspace root */
    base_dir?: string;
  };
  baseline: {
    variant_id: string;
    bindings: Bindings;
  };
  variant_plan: Array<{
    variant_id: string;
    bindings: Bindings;
  }>;
  benchmark?: {
    policy?: BenchmarkTypePolicy;
    adapter?: BenchmarkAdapterConfig;
  };
  runtime: {
    harness: {
      mode: 'cli';
      command: string[];
      integration_level: string;
      input_path: string;
      output_path: string;
      control_plane: {
        mode: 'file';
        path: string;
      };
    };
    sandbox: {
      mode: 'container' | 'local';
      engine?: 'docker';
      image?: string;
      root_read_only?: boolean;
      run_as_user?: string;
      hardening?: {
        no_new_privileges: boolean;
        drop_all_caps: boolean;
      };
      resources?: {
        cpu_count: number;
        memory_mb: number;
      };
    };
    network: {
      mode: 'none' | 'full' | 'allowlist_enforced';
      allowed_hosts: string[];
    };
  };
  validity: {
    fail_on_state_leak: boolean;
    fail_on_profile_invariant_violation: boolean;
  };
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

export class ExperimentBuilder {
  private readonly spec: ExperimentSpec;

  /** Create a builder with explicit id/name (defaults to variant_sequential). */
  static create(id: string, name: string): ExperimentBuilder {
    return new ExperimentBuilder(id, name);
  }

  /** Create a builder from a policy bundle (preset or custom). Set id/name via .id()/.name(). */
  static from(policies: DesignPolicies): ExperimentBuilder {
    const builder = new ExperimentBuilder('', '');
    builder.spec.design.policies = copyPolicies(policies);
    builder.spec.design.comparison = policies.comparison;
    return builder;
  }

  private constructor(id: string, name: string) {
    this.spec = {
      version: '0.3',
      experiment: {
        id,
        name,
        tags: [],
      },
      dataset: {
        suite_id: '',
        provider: 'local_jsonl',
        path: '',
        schema_version: 'task_jsonl_v1',
        split_id: '',
        limit: 0,
      },
      design: {
        sanitization_profile: 'hermetic_functional_v2',
        comparison: 'paired',
        replications: 1,
        random_seed: 1,
        shuffle_tasks: true,
        max_concurrency: 1,
      },
      metrics: [],
      baseline: {
        variant_id: 'base',
        bindings: {},
      },
      variant_plan: [],
      runtime: {
        harness: {
          mode: 'cli',
          command: [],
          integration_level: '',
          input_path: '/out/trial_input.json',
          output_path: '/out/trial_output.json',
          control_plane: {
            mode: 'file',
            path: '/state/lab_control.json',
          },
        },
        sandbox: { mode: 'local' },
        network: {
          mode: 'none',
          allowed_hosts: [],
        },
      },
      validity: {
        fail_on_state_leak: true,
        fail_on_profile_invariant_violation: true,
      },
    };
  }

  id(value: string): this {
    this.spec.experiment.id = value;
    return this;
  }

  name(value: string): this {
    this.spec.experiment.name = value;
    return this;
  }

  description(value: string): this {
    this.spec.experiment.description = value;
    return this;
  }

  owner(value: string): this {
    this.spec.experiment.owner = value;
    return this;
  }

  tags(values: string[]): this {
    this.spec.experiment.tags = [...values];
    return this;
  }

  datasetJsonl(path: string, options: DatasetJsonlOptions): this {
    this.spec.dataset.path = path;
    this.spec.dataset.suite_id = options.suiteId;
    this.spec.dataset.provider = options.provider ?? 'local_jsonl';
    this.spec.dataset.schema_version = options.schemaVersion ?? this.spec.dataset.schema_version;
    this.spec.dataset.split_id = options.splitId;
    this.spec.dataset.limit = options.limit;
    return this;
  }

  harnessCli(command: string[], options: HarnessCliOptions): this {
    this.spec.runtime.harness.command = [...command];
    this.spec.runtime.harness.integration_level = options.integrationLevel;
    this.spec.runtime.harness.input_path = options.inputPath ?? this.spec.runtime.harness.input_path;
    this.spec.runtime.harness.output_path = options.outputPath ?? this.spec.runtime.harness.output_path;
    return this;
  }

  baseline(variantId: string, bindings: Bindings): this {
    this.spec.baseline = { variant_id: variantId, bindings: { ...bindings } };
    return this;
  }

  addVariant(variantId: string, bindings: Bindings): this {
    this.spec.variant_plan.push({ variant_id: variantId, bindings: { ...bindings } });
    return this;
  }

  benchmark(config: BenchmarkConfig): this {
    this.spec.benchmark = {
      policy: config.policy
        ? {
            ...config.policy,
            required_evidence_classes: config.policy.required_evidence_classes
              ? [...config.policy.required_evidence_classes]
              : undefined,
          }
        : undefined,
      adapter: config.adapter
        ? {
            command: [...config.adapter.command],
            manifest: config.adapter.manifest
              ? JSON.parse(JSON.stringify(config.adapter.manifest)) as Record<string, unknown>
              : undefined,
          }
        : undefined,
    };
    return this;
  }

  replications(value: number): this {
    this.spec.design.replications = value;
    return this;
  }

  sanitizationProfile(value: string): this {
    this.spec.design.sanitization_profile = value;
    return this;
  }

  randomSeed(value: number): this {
    this.spec.design.random_seed = value;
    return this;
  }

  maxConcurrency(value: number): this {
    this.spec.design.max_concurrency = value;
    return this;
  }

  /** Set design policies directly (overrides any preset from .from()). */
  policies(value: DesignPolicies): this {
    this.spec.design.policies = copyPolicies(value);
    this.spec.design.comparison = value.comparison;
    return this;
  }

  /** Add a metric definition. Use Metric.* constants or Metric.fromOutput() / Metric.fromEvents(). */
  metric(def: MetricDef): this {
    // Replace existing metric with same id (allows overriding predefined defs)
    const idx = this.spec.metrics.findIndex((m) => m.id === def.id);
    if (idx >= 0) {
      this.spec.metrics[idx] = { ...def };
    } else {
      this.spec.metrics.push({ ...def });
    }
    return this;
  }

  /** Add a budget guardrail. Use Metric.max*() factories for common limits. */
  guardrail(def: GuardrailDef): this {
    if (!this.spec.guardrails) {
      this.spec.guardrails = [];
    }
    const idx = this.spec.guardrails.findIndex((g) => g.metric_id === def.metric_id);
    if (idx >= 0) {
      this.spec.guardrails[idx] = { ...def };
    } else {
      this.spec.guardrails.push({ ...def });
    }
    return this;
  }

  /** Configure artifact collection from the workspace after each trial. */
  artifacts(options: { collect: string[]; diff?: boolean; baseDir?: string }): this {
    this.spec.artifacts = {
      collect: [...options.collect],
      diff: options.diff ?? false,
      base_dir: options.baseDir,
    };
    return this;
  }

  networkMode(mode: 'none' | 'full' | 'allowlist_enforced', allowedHosts: string[] = []): this {
    this.spec.runtime.network.mode = mode;
    this.spec.runtime.network.allowed_hosts = [...allowedHosts];
    return this;
  }

  sandboxImage(image: string): this {
    this.spec.runtime.sandbox.mode = 'container';
    this.spec.runtime.sandbox.image = image;
    return this;
  }

  localSandbox(): this {
    this.spec.runtime.sandbox = { mode: 'local' };
    return this;
  }

  build(): ExperimentSpec {
    const missing: string[] = [];
    if (!this.spec.experiment.id) missing.push('experiment id (call .id() or use ExperimentBuilder.create())');
    if (!this.spec.experiment.name) missing.push('experiment name (call .name() or use ExperimentBuilder.create())');
    if (!this.spec.dataset.path) missing.push('dataset path (call .datasetJsonl())');
    if (!this.spec.dataset.suite_id) missing.push('dataset suite_id (call .datasetJsonl() with suiteId)');
    if (!this.spec.dataset.split_id) missing.push('dataset split_id (call .datasetJsonl() with splitId)');
    if (this.spec.dataset.limit <= 0) missing.push('dataset limit (call .datasetJsonl() with limit > 0)');
    if (this.spec.runtime.harness.command.length === 0) missing.push('harness command (call .harnessCli())');
    if (!this.spec.runtime.harness.integration_level) missing.push('harness integration_level (call .harnessCli() with integrationLevel)');
    if (missing.length > 0) {
      throw new Error(
        `ExperimentBuilder: required fields not set:\n${missing.map((m) => `  - ${m}`).join('\n')}`,
      );
    }

    // Policy coherence validation
    const policies = this.spec.design.policies;
    if (policies) {
      const errors: string[] = [];
      const treatmentCount = this.spec.variant_plan.length;
      const totalVariants = treatmentCount + 1; // baseline + treatments

      if (policies.comparison === 'paired' && totalVariants < 2) {
        errors.push('paired comparison requires at least one treatment variant (call .addVariant())');
      }
      if (policies.scheduling === 'paired_interleaved' && totalVariants < 2) {
        errors.push('paired_interleaved scheduling requires at least 2 variants');
      }
      if (policies.retry.max_attempts < 1) {
        errors.push('retry.max_attempts must be >= 1');
      }
      if (errors.length > 0) {
        throw new Error(
          `ExperimentBuilder: policy coherence errors:\n${errors.map((e) => `  - ${e}`).join('\n')}`,
        );
      }
    }

    return JSON.parse(JSON.stringify(this.spec)) as ExperimentSpec;
  }

  toYaml(): string {
    return yamlStringify(this.build());
  }
}
