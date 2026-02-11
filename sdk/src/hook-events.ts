// ---------------------------------------------------------------------------
// Typed Event Stream
// Mirrors hook_events_v1.jsonschema — discriminated union of 6 event types
// ---------------------------------------------------------------------------

import type { TrialIds } from './trial-output.js';

// ---------------------------------------------------------------------------
// Shared sub-types
// ---------------------------------------------------------------------------

export interface CallOutcome {
  status: 'ok' | 'error';
  error_type?: string;
  message?: string;
}

export interface StepBudgets {
  steps: number;
  tokens_in: number;
  tokens_out: number;
  tool_calls: number;
}

export interface ModelIdentity {
  identity: string;
  params_digest?: string;
}

export interface CallTiming {
  queue_wait_ms?: number;
  duration_ms: number;
}

export interface RedactionInfo {
  applied: boolean;
  mode: 'store' | 'hash' | 'drop';
}

// ---------------------------------------------------------------------------
// Base fields (shared by all event types)
// ---------------------------------------------------------------------------

export interface HookEventBase {
  hooks_schema_version: 'hook_events_v1';
  ts: string;
  seq: number;
  ids: TrialIds;
  step_index?: number | null;
  payload_ref?: string;
  redaction?: RedactionInfo;
  ext?: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// 6 concrete event types
// ---------------------------------------------------------------------------

export interface AgentStepStartEvent extends HookEventBase {
  event_type: 'agent_step_start';
  step_index: number;
}

export interface AgentStepEndEvent extends HookEventBase {
  event_type: 'agent_step_end';
  step_index: number;
  budgets?: StepBudgets;
}

export interface ModelCallEndEvent extends HookEventBase {
  event_type: 'model_call_end';
  call_id: string;
  outcome: CallOutcome;
  turn_index?: number;
  model?: ModelIdentity;
  usage?: {
    tokens_in: number;
    tokens_out: number;
  };
  timing?: CallTiming;
  attempt_index?: number;
}

export interface ToolCallEndEvent extends HookEventBase {
  event_type: 'tool_call_end';
  call_id: string;
  tool: {
    name: string;
    version?: string;
  };
  outcome: CallOutcome;
  timing?: CallTiming;
  attempt_index?: number;
}

export type ControlAction = 'continue' | 'stop' | 'checkpoint';

export interface ControlAckEvent extends HookEventBase {
  event_type: 'control_ack';
  step_index: number;
  /** SHA-256 digest of the control file, e.g. "sha256:abc123..." */
  control_version: string;
  control_seq?: number;
  action_observed: ControlAction;
  action_taken?: ControlAction;
  reason?: string;
}

export interface ErrorEvent extends HookEventBase {
  event_type: 'error';
  message: string;
  error_type?: string;
  stack?: string;
}

// ---------------------------------------------------------------------------
// Discriminated union
// ---------------------------------------------------------------------------

export type HookEvent =
  | AgentStepStartEvent
  | AgentStepEndEvent
  | ModelCallEndEvent
  | ToolCallEndEvent
  | ControlAckEvent
  | ErrorEvent;
