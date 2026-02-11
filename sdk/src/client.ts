import { spawn } from 'node:child_process';
import { resolve } from 'node:path';

import type {
  CommandOptions,
  DescribeArgs,
  DescribeResponse,
  ForkArgs,
  ForkResponse,
  HooksValidateArgs,
  JsonMap,
  JsonCommandResponse,
  KnobsValidateArgs,
  LabClientOptions,
  LabErrorEnvelope,
  PauseArgs,
  PauseResponse,
  PublishArgs,
  PublishResponse,
  ReplayArgs,
  ReplayResponse,
  ResumeArgs,
  ResumeResponse,
  RunArgs,
  RunDevArgs,
  RunExperimentArgs,
  RunResponse,
  SchemaValidateArgs,
  ValidateResponse,
} from './types.js';

export class LabRunnerError extends Error {
  readonly code: string;
  readonly details?: unknown;
  readonly exitCode?: number;
  readonly command: string[];
  readonly stderr: string;

  constructor(params: {
    message: string;
    code: string;
    command: string[];
    stderr: string;
    details?: unknown;
    exitCode?: number;
  }) {
    super(params.message);
    this.name = 'LabRunnerError';
    this.code = params.code;
    this.details = params.details;
    this.exitCode = params.exitCode;
    this.command = params.command;
    this.stderr = params.stderr;
  }
}

export class LabClient {
  private readonly runnerBin: string;
  private readonly cwd: string;
  private readonly env: NodeJS.ProcessEnv;

  constructor(options: LabClientOptions = {}) {
    this.runnerBin = options.runnerBin ?? process.env.AGENTLAB_RUNNER_BIN ?? 'lab';
    this.cwd = options.cwd ? resolve(options.cwd) : process.cwd();
    this.env = { ...process.env, ...(options.env ?? {}) };
  }

  async describe(args: DescribeArgs): Promise<DescribeResponse> {
    const cmd = ['describe', args.experiment, '--json'];
    if (args.overrides) {
      cmd.push('--overrides', args.overrides);
    }
    return this.runJson<DescribeResponse>(cmd, args);
  }

  async run(args: RunArgs): Promise<RunResponse> {
    const cmd = ['run', args.experiment, '--json'];
    if (args.container) {
      cmd.push('--container');
    }
    if (args.overrides) {
      cmd.push('--overrides', args.overrides);
    }
    return this.runJson<RunResponse>(cmd, args);
  }

  async runDev(args: RunDevArgs): Promise<RunResponse> {
    const cmd = ['run-dev', args.experiment, '--json'];
    if (args.setup) {
      cmd.push('--setup', args.setup);
    }
    if (args.overrides) {
      cmd.push('--overrides', args.overrides);
    }
    return this.runJson<RunResponse>(cmd, args);
  }

  async runExperiment(args: RunExperimentArgs): Promise<RunResponse> {
    const cmd = ['run-experiment', args.experiment, '--json'];
    if (args.overrides) {
      cmd.push('--overrides', args.overrides);
    }
    return this.runJson<RunResponse>(cmd, args);
  }

  async replay(args: ReplayArgs): Promise<ReplayResponse> {
    const cmd = ['replay', '--run-dir', args.runDir, '--trial-id', args.trialId, '--json'];
    if (args.strict) {
      cmd.push('--strict');
    }
    return this.runJson<ReplayResponse>(cmd, args);
  }

  async fork(args: ForkArgs): Promise<ForkResponse> {
    const cmd = [
      'fork',
      '--run-dir',
      args.runDir,
      '--from-trial',
      args.fromTrial,
      '--at',
      args.at,
      '--json',
    ];
    appendSetBindings(cmd, args.set);
    if (args.strict) {
      cmd.push('--strict');
    }
    return this.runJson<ForkResponse>(cmd, args);
  }

  async pause(args: PauseArgs): Promise<PauseResponse> {
    const cmd = ['pause', '--run-dir', args.runDir, '--json'];
    if (args.trialId) {
      cmd.push('--trial-id', args.trialId);
    }
    if (args.label) {
      cmd.push('--label', args.label);
    }
    if (typeof args.timeoutSeconds === 'number') {
      cmd.push('--timeout-seconds', String(args.timeoutSeconds));
    }
    return this.runJson<PauseResponse>(cmd, args);
  }

  async resume(args: ResumeArgs): Promise<ResumeResponse> {
    const cmd = ['resume', '--run-dir', args.runDir, '--json'];
    if (args.trialId) {
      cmd.push('--trial-id', args.trialId);
    }
    if (args.label) {
      cmd.push('--label', args.label);
    }
    appendSetBindings(cmd, args.set);
    if (args.strict) {
      cmd.push('--strict');
    }
    return this.runJson<ResumeResponse>(cmd, args);
  }

  async publish(args: PublishArgs): Promise<PublishResponse> {
    const cmd = ['publish', '--run-dir', args.runDir, '--json'];
    if (args.out) {
      cmd.push('--out', args.out);
    }
    return this.runJson<PublishResponse>(cmd, args);
  }

  async validateKnobs(args: KnobsValidateArgs): Promise<ValidateResponse> {
    const cmd = [
      'knobs-validate',
      '--manifest',
      args.manifest,
      '--overrides',
      args.overrides,
      '--json',
    ];
    return this.runJson<ValidateResponse>(cmd, args);
  }

  async validateHooks(args: HooksValidateArgs): Promise<ValidateResponse> {
    const cmd = [
      'hooks-validate',
      '--manifest',
      args.manifest,
      '--events',
      args.events,
      '--json',
    ];
    return this.runJson<ValidateResponse>(cmd, args);
  }

  async validateSchema(args: SchemaValidateArgs): Promise<ValidateResponse> {
    const cmd = ['schema-validate', '--schema', args.schema, '--file', args.file, '--json'];
    return this.runJson<ValidateResponse>(cmd, args);
  }

  private async runJson<T extends JsonCommandResponse>(args: string[], options?: CommandOptions): Promise<T> {
    const result = await this.spawnCommand(args, options);
    const payload = this.parsePayload(result.stdout, result.stderr, args);

    if (this.isErrorEnvelope(payload)) {
      const errPayload = payload;
      throw new LabRunnerError({
        message: errPayload.error.message,
        code: errPayload.error.code,
        details: errPayload.error.details,
        command: [this.runnerBin, ...args],
        stderr: result.stderr,
        exitCode: result.exitCode,
      });
    }

    return payload as T;
  }

  private isErrorEnvelope(payload: unknown): payload is LabErrorEnvelope {
    if (!payload || typeof payload !== 'object') {
      return false;
    }
    const obj = payload as { ok?: unknown; error?: unknown };
    return (
      Object.prototype.hasOwnProperty.call(obj, 'ok') &&
      obj.ok === false &&
      typeof obj.error === 'object' &&
      obj.error !== null
    );
  }

  private spawnCommand(
    args: string[],
    options?: CommandOptions,
  ): Promise<{ stdout: string; stderr: string; exitCode: number }> {
    const cwd = options?.cwd ? resolve(this.cwd, options.cwd) : this.cwd;
    const env = { ...this.env, ...(options?.env ?? {}) };

    return new Promise((resolvePromise, rejectPromise) => {
      const child = spawn(this.runnerBin, args, {
        cwd,
        env,
        stdio: ['ignore', 'pipe', 'pipe'],
      });

      let stdout = '';
      let stderr = '';

      child.stdout.on('data', (chunk) => {
        stdout += chunk.toString();
      });
      child.stderr.on('data', (chunk) => {
        stderr += chunk.toString();
      });

      child.on('error', (error) => {
        rejectPromise(
          new LabRunnerError({
            message: error.message,
            code: 'spawn_failed',
            command: [this.runnerBin, ...args],
            stderr,
          }),
        );
      });

      child.on('close', (exitCode) => {
        const code = exitCode ?? 1;
        if (code !== 0 && !stdout.trim()) {
          rejectPromise(
            new LabRunnerError({
              message: stderr.trim() || `Runner exited with code ${code}`,
              code: 'runner_exit_nonzero',
              command: [this.runnerBin, ...args],
              stderr,
              exitCode: code,
            }),
          );
          return;
        }
        resolvePromise({ stdout, stderr, exitCode: code });
      });
    });
  }

  private parsePayload(stdout: string, stderr: string, args: string[]): unknown {
    const lines = stdout
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
    const candidate = lines.length > 0 ? lines[lines.length - 1] : stderr.trim();
    if (!candidate) {
      throw new LabRunnerError({
        message: 'Runner produced no JSON payload',
        code: 'empty_payload',
        command: [this.runnerBin, ...args],
        stderr,
      });
    }

    let parsed: unknown;
    try {
      parsed = JSON.parse(candidate);
    } catch {
      throw new LabRunnerError({
        message: 'Failed to parse runner JSON payload',
        code: 'invalid_json',
        command: [this.runnerBin, ...args],
        stderr,
        details: { candidate },
      });
    }

    if (!parsed || typeof parsed !== 'object') {
      throw new LabRunnerError({
        message: 'Runner JSON payload is not an object',
        code: 'invalid_payload',
        command: [this.runnerBin, ...args],
        stderr,
        details: { payload: parsed },
      });
    }

    return parsed;
  }
}

function appendSetBindings(args: string[], bindings?: JsonMap): void {
  if (!bindings) {
    return;
  }

  for (const [key, value] of Object.entries(bindings)) {
    if (!key.trim()) {
      throw new Error('set bindings must use non-empty keys');
    }
    const encoded = JSON.stringify(value);
    if (encoded === undefined) {
      throw new Error(`set binding "${key}" cannot be undefined`);
    }
    args.push('--set', `${key}=${encoded}`);
  }
}
