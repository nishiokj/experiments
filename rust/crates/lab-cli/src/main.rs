use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "lab", version = "0.3.0", about = "AgentLab Rust CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExecutorArg {
    #[value(name = "local_docker")]
    LocalDocker,
    #[value(name = "local_process")]
    LocalProcess,
    #[value(name = "remote")]
    Remote,
}

impl From<ExecutorArg> for lab_runner::ExecutorKind {
    fn from(value: ExecutorArg) -> Self {
        match value {
            ExecutorArg::LocalDocker => lab_runner::ExecutorKind::LocalDocker,
            ExecutorArg::LocalProcess => lab_runner::ExecutorKind::LocalProcess,
            ExecutorArg::Remote => lab_runner::ExecutorKind::Remote,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MaterializeArg {
    #[value(name = "none")]
    None,
    #[value(name = "metadata_only")]
    MetadataOnly,
    #[value(name = "outputs_only")]
    OutputsOnly,
    #[value(name = "full")]
    Full,
}

impl From<MaterializeArg> for lab_runner::MaterializationMode {
    fn from(value: MaterializeArg) -> Self {
        match value {
            MaterializeArg::None => lab_runner::MaterializationMode::None,
            MaterializeArg::MetadataOnly => lab_runner::MaterializationMode::MetadataOnly,
            MaterializeArg::OutputsOnly => lab_runner::MaterializationMode::OutputsOnly,
            MaterializeArg::Full => lab_runner::MaterializationMode::Full,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    Run {
        experiment: PathBuf,
        #[arg(long)]
        container: bool,
        #[arg(long, value_enum)]
        executor: Option<ExecutorArg>,
        #[arg(long, value_enum)]
        materialize: Option<MaterializeArg>,
        #[arg(long)]
        remote_endpoint: Option<String>,
        #[arg(long)]
        remote_token_env: Option<String>,
        #[arg(long)]
        overrides: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    RunDev {
        experiment: PathBuf,
        #[arg(long)]
        setup: Option<String>,
        #[arg(long)]
        overrides: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    RunExperiment {
        experiment: PathBuf,
        #[arg(long)]
        overrides: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Replay {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        trial_id: String,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        json: bool,
    },
    Fork {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        from_trial: String,
        #[arg(long)]
        at: String,
        #[arg(long = "set")]
        set_values: Vec<String>,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        json: bool,
    },
    Pause {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        trial_id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value_t = 60)]
        timeout_seconds: u64,
        #[arg(long)]
        json: bool,
    },
    Resume {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        trial_id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long = "set")]
        set_values: Vec<String>,
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        json: bool,
    },
    Describe {
        experiment: PathBuf,
        #[arg(long)]
        overrides: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    KnobsInit {
        #[arg(long, default_value = ".lab/knobs/manifest.json")]
        manifest: PathBuf,
        #[arg(long, default_value = ".lab/knobs/overrides.json")]
        overrides: PathBuf,
        #[arg(long)]
        force: bool,
    },
    KnobsValidate {
        #[arg(long, default_value = ".lab/knobs/manifest.json")]
        manifest: PathBuf,
        #[arg(long, default_value = ".lab/knobs/overrides.json")]
        overrides: PathBuf,
        #[arg(long)]
        json: bool,
    },
    SchemaValidate {
        #[arg(long)]
        schema: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    HooksValidate {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        events: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Publish {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Init {
        #[arg(long)]
        in_place: bool,
        #[arg(long)]
        force: bool,
    },
    Clean {
        #[arg(long)]
        init: bool,
        #[arg(long)]
        runs: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let json_mode = command_json_mode(&cli.command);
    let result = run_command(cli.command);
    match result {
        Ok(Some(payload)) => {
            emit_json(&payload);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(err) => {
            if json_mode {
                emit_json(&json_error("command_failed", err.to_string(), json!({})));
                std::process::exit(1);
            }
            Err(err)
        }
    }
}

fn run_command(command: Commands) -> Result<Option<Value>> {
    match command {
        Commands::Run {
            experiment,
            container,
            executor,
            materialize,
            remote_endpoint,
            remote_token_env,
            overrides,
            json,
        } => {
            let summary =
                lab_runner::describe_experiment_with_overrides(&experiment, overrides.as_deref())?;
            let execution = lab_runner::RunExecutionOptions {
                executor: executor.map(Into::into),
                materialize: materialize.map(Into::into),
                remote_endpoint,
                remote_token_env,
            };
            let result = lab_runner::run_experiment_with_options_and_overrides(
                &experiment,
                container,
                overrides.as_deref(),
                execution.clone(),
            )?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "run",
                    "summary": summary_to_json(&summary),
                    "run": run_result_to_json(&result),
                    "container": container,
                    "executor": execution.executor.map(|e| e.as_str()),
                    "materialize": execution.materialize.map(|m| m.as_str()),
                    "remote_endpoint": execution.remote_endpoint,
                    "remote_token_env": execution.remote_token_env
                })));
            }
            print_summary(&summary);
            println!("run_id: {}", result.run_id);
            println!("run_dir: {}", result.run_dir.display());
        }
        Commands::RunDev {
            experiment,
            setup,
            overrides,
            json,
        } => {
            let summary =
                lab_runner::describe_experiment_with_overrides(&experiment, overrides.as_deref())?;
            let setup_for_json = setup.clone();
            let result = lab_runner::run_experiment_dev_with_overrides(
                &experiment,
                setup.clone(),
                overrides.as_deref(),
            )?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "run-dev",
                    "summary": summary_to_json(&summary),
                    "run": run_result_to_json(&result),
                    "dev_setup": setup_for_json,
                    "dev_network_mode": "full"
                })));
            }
            print_summary(&summary);
            if let Some(s) = &setup {
                println!("dev_setup: {}", s);
            } else {
                println!("dev_setup: none");
            }
            println!("dev_network_mode: full");
            println!("run_id: {}", result.run_id);
            println!("run_dir: {}", result.run_dir.display());
        }
        Commands::RunExperiment {
            experiment,
            overrides,
            json,
        } => {
            let summary =
                lab_runner::describe_experiment_with_overrides(&experiment, overrides.as_deref())?;
            let result = lab_runner::run_experiment_strict_with_overrides(
                &experiment,
                overrides.as_deref(),
            )?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "run-experiment",
                    "summary": summary_to_json(&summary),
                    "run": run_result_to_json(&result),
                    "experiment_network_requirement": "none"
                })));
            }
            print_summary(&summary);
            println!("experiment_network_requirement: none");
            println!("run_id: {}", result.run_id);
            println!("run_dir: {}", result.run_dir.display());
        }
        Commands::Replay {
            run_dir,
            trial_id,
            strict,
            json,
        } => {
            let result = lab_runner::replay_trial(&run_dir, &trial_id, strict)?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "replay",
                    "replay": replay_result_to_json(&result),
                })));
            }
            println!("replay_id: {}", result.replay_id);
            println!("replay_dir: {}", result.replay_dir.display());
            println!("parent_trial_id: {}", result.parent_trial_id);
            println!("strict: {}", result.strict);
            println!("replay_grade: {}", result.replay_grade);
            println!("harness_status: {}", result.harness_status);
        }
        Commands::Fork {
            run_dir,
            from_trial,
            at,
            set_values,
            strict,
            json,
        } => {
            let set_bindings = parse_set_bindings(&set_values)?;
            let result = lab_runner::fork_trial(&run_dir, &from_trial, &at, &set_bindings, strict)?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "fork",
                    "fork": fork_result_to_json(&result),
                })));
            }
            println!("fork_id: {}", result.fork_id);
            println!("fork_dir: {}", result.fork_dir.display());
            println!("parent_trial_id: {}", result.parent_trial_id);
            println!("selector: {}", result.selector);
            println!("strict: {}", result.strict);
            println!(
                "source_checkpoint: {}",
                result.source_checkpoint.as_deref().unwrap_or("none")
            );
            println!("fallback_mode: {}", result.fallback_mode);
            println!("replay_grade: {}", result.replay_grade);
            println!("harness_status: {}", result.harness_status);
        }
        Commands::Pause {
            run_dir,
            trial_id,
            label,
            timeout_seconds,
            json,
        } => {
            let result = lab_runner::pause_run(
                &run_dir,
                trial_id.as_deref(),
                label.as_deref(),
                timeout_seconds,
            )?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "pause",
                    "pause": pause_result_to_json(&result),
                })));
            }
            println!("run_id: {}", result.run_id);
            println!("trial_id: {}", result.trial_id);
            println!("label: {}", result.label);
            println!("checkpoint_acked: {}", result.checkpoint_acked);
            println!("stop_acked: {}", result.stop_acked);
        }
        Commands::Resume {
            run_dir,
            trial_id,
            label,
            set_values,
            strict,
            json,
        } => {
            let set_bindings = parse_set_bindings(&set_values)?;
            let result = lab_runner::resume_run(
                &run_dir,
                trial_id.as_deref(),
                label.as_deref(),
                &set_bindings,
                strict,
            )?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "resume",
                    "resume": resume_result_to_json(&result),
                })));
            }
            println!("trial_id: {}", result.trial_id);
            println!("selector: {}", result.selector);
            println!("fork_id: {}", result.fork.fork_id);
            println!("fork_dir: {}", result.fork.fork_dir.display());
            println!("replay_grade: {}", result.fork.replay_grade);
            println!("harness_status: {}", result.fork.harness_status);
        }
        Commands::Describe {
            experiment,
            overrides,
            json,
        } => {
            let summary =
                lab_runner::describe_experiment_with_overrides(&experiment, overrides.as_deref())?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "describe",
                    "summary": summary_to_json(&summary)
                })));
            }
            print_summary(&summary);
        }
        Commands::KnobsInit {
            manifest,
            overrides,
            force,
        } => {
            write_knob_files(&manifest, &overrides, force)?;
            println!("wrote: {}", manifest.display());
            println!("wrote: {}", overrides.display());
            println!(
                "next: lab knobs-validate --manifest {} --overrides {}",
                manifest.display(),
                overrides.display()
            );
        }
        Commands::KnobsValidate {
            manifest,
            overrides,
            json,
        } => {
            lab_runner::validate_knob_overrides(&manifest, &overrides)?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "knobs-validate",
                    "valid": true,
                    "manifest": manifest.display().to_string(),
                    "overrides": overrides.display().to_string()
                })));
            }
            println!("ok");
        }
        Commands::SchemaValidate { schema, file, json } => {
            let compiled = lab_schemas::compile_schema(&schema)?;
            let data = std::fs::read_to_string(file)?;
            let value: serde_json::Value = serde_json::from_str(&data)?;
            if let Err(errors) = compiled.validate(&value) {
                for e in errors {
                    eprintln!("schema error: {}", e);
                }
                std::process::exit(1);
            }
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "schema-validate",
                    "valid": true,
                    "schema": schema
                })));
            }
            println!("ok");
        }
        Commands::HooksValidate {
            manifest,
            events,
            json,
        } => {
            let man = lab_hooks::load_manifest(&manifest)?;
            let schema = lab_schemas::compile_schema("hook_events_v1.jsonschema")?;
            lab_hooks::validate_hooks(&man, &events, &schema)?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "hooks-validate",
                    "valid": true,
                    "manifest": manifest.display().to_string(),
                    "events": events.display().to_string()
                })));
            }
            println!("ok");
        }
        Commands::Publish { run_dir, out, json } => {
            let out_path = out.unwrap_or(run_dir.join("debug_bundles").join("bundle.zip"));
            std::fs::create_dir_all(out_path.parent().unwrap())?;
            lab_provenance::build_debug_bundle(&run_dir, &out_path)?;
            if json {
                return Ok(Some(json!({
                    "ok": true,
                    "command": "publish",
                    "bundle": out_path.display().to_string(),
                    "run_dir": run_dir.display().to_string()
                })));
            }
            println!("bundle: {}", out_path.display());
        }
        Commands::Init { in_place, force } => {
            let cwd = std::env::current_dir()?;
            let root = cwd;
            let lab_dir = root.join(".lab");
            std::fs::create_dir_all(&lab_dir)?;

            let exp_path = if in_place {
                root.join("experiment.yaml")
            } else {
                lab_dir.join("experiment.yaml")
            };

            if !force && exp_path.exists() {
                return Err(anyhow::anyhow!(format!(
                    "init file already exists (use --force): {}",
                    exp_path.display()
                )));
            }

            let exp_yaml = "\
version: '0.3'
experiment:
  id: ''                              # REQUIRED
  name: ''                            # REQUIRED
  workload_type: ''                   # REQUIRED: agent_harness | trainer
dataset:
  path: ''                            # REQUIRED: path to tasks.jsonl
  provider: local_jsonl
  suite_id: ''                        # REQUIRED
  schema_version: task_jsonl_v1
  split_id: ''                        # REQUIRED
  limit: 0                            # REQUIRED: set > 0
design:
  sanitization_profile: ''            # REQUIRED: e.g. hermetic_functional_v2
  comparison: paired
  replications: 0                     # REQUIRED: set > 0
  random_seed: 0                      # REQUIRED
  shuffle_tasks: true
  max_concurrency: 1
baseline:
  variant_id: ''                      # REQUIRED
  bindings: {}
variant_plan: []
runtime:
  harness:
    mode: cli
    command: []                        # REQUIRED: e.g. [node, ./harness.js, run]
    integration_level: ''              # REQUIRED: cli_basic | cli_events | otel | sdk_control | sdk_full
    input_path: /out/trial_input.json
    output_path: /out/trial_output.json
    control_plane:
      mode: file
      path: /state/lab_control.json
  sandbox:
    mode: local
  network:
    mode: none
    allowed_hosts: []
validity:
  fail_on_state_leak: true
  fail_on_profile_invariant_violation: true
";
            std::fs::write(&exp_path, exp_yaml)?;

            let exp_show = exp_path.strip_prefix(&root).unwrap_or(&exp_path).display();
            println!("wrote: {}", exp_show);
            println!(
                "next: edit {} \u{2014} fill in all fields marked REQUIRED",
                exp_show
            );
            println!("next: lab describe {}", exp_show);
        }
        Commands::Clean { init, runs } => {
            let root = std::env::current_dir()?;
            let lab_dir = root.join(".lab");
            if init {
                let candidates = vec![
                    root.join("experiment.yaml"),
                    lab_dir.join("experiment.yaml"),
                ];
                for p in candidates {
                    if p.exists() {
                        let _ = std::fs::remove_file(&p);
                        println!("removed: {}", p.display());
                    }
                }
            }
            if runs {
                let runs_dir = lab_dir.join("runs");
                if runs_dir.exists() {
                    std::fs::remove_dir_all(&runs_dir)?;
                    println!("removed: {}", runs_dir.display());
                }
            }
        }
    }
    Ok(None)
}

fn emit_json(value: &Value) {
    match serde_json::to_string(value) {
        Ok(s) => println!("{}", s),
        Err(_) => println!(
            "{{\"ok\":false,\"error\":{{\"code\":\"serialization_error\",\"message\":\"failed to serialize JSON payload\",\"details\":{{}}}}}}"
        ),
    }
}

fn json_error(code: &str, message: String, details: Value) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "details": details
        }
    })
}

fn command_json_mode(command: &Commands) -> bool {
    match command {
        Commands::Run { json, .. }
        | Commands::RunDev { json, .. }
        | Commands::RunExperiment { json, .. }
        | Commands::Replay { json, .. }
        | Commands::Fork { json, .. }
        | Commands::Pause { json, .. }
        | Commands::Resume { json, .. }
        | Commands::Describe { json, .. }
        | Commands::KnobsValidate { json, .. }
        | Commands::SchemaValidate { json, .. }
        | Commands::HooksValidate { json, .. }
        | Commands::Publish { json, .. } => *json,
        _ => false,
    }
}

fn run_result_to_json(result: &lab_runner::RunResult) -> Value {
    json!({
        "run_id": result.run_id,
        "run_dir": result.run_dir.display().to_string()
    })
}

fn replay_result_to_json(result: &lab_runner::ReplayResult) -> Value {
    json!({
        "replay_id": result.replay_id,
        "replay_dir": result.replay_dir.display().to_string(),
        "parent_trial_id": result.parent_trial_id,
        "strict": result.strict,
        "replay_grade": result.replay_grade,
        "harness_status": result.harness_status,
    })
}

fn fork_result_to_json(result: &lab_runner::ForkResult) -> Value {
    json!({
        "fork_id": result.fork_id,
        "fork_dir": result.fork_dir.display().to_string(),
        "parent_trial_id": result.parent_trial_id,
        "selector": result.selector,
        "strict": result.strict,
        "source_checkpoint": result.source_checkpoint,
        "fallback_mode": result.fallback_mode,
        "replay_grade": result.replay_grade,
        "harness_status": result.harness_status,
    })
}

fn pause_result_to_json(result: &lab_runner::PauseResult) -> Value {
    json!({
        "run_id": result.run_id,
        "trial_id": result.trial_id,
        "label": result.label,
        "checkpoint_acked": result.checkpoint_acked,
        "stop_acked": result.stop_acked,
    })
}

fn resume_result_to_json(result: &lab_runner::ResumeResult) -> Value {
    json!({
        "trial_id": result.trial_id,
        "selector": result.selector,
        "fork": fork_result_to_json(&result.fork),
    })
}

fn parse_set_bindings(values: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for raw in values {
        let (key, val_raw) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!(format!("invalid --set '{}': expected k=v", raw)))?;
        if key.trim().is_empty() {
            return Err(anyhow::anyhow!(format!(
                "invalid --set '{}': key cannot be empty",
                raw
            )));
        }
        let parsed =
            serde_json::from_str::<Value>(val_raw).unwrap_or(Value::String(val_raw.to_string()));
        out.insert(key.to_string(), parsed);
    }
    Ok(out)
}

fn summary_to_json(summary: &lab_runner::ExperimentSummary) -> Value {
    json!({
        "experiment": summary.exp_id,
        "workload_type": summary.workload_type,
        "dataset": summary.dataset_path.display().to_string(),
        "tasks": summary.task_count,
        "replications": summary.replications,
        "variant_plan_entries": summary.variant_count,
        "total_trials": summary.total_trials,
        "harness": summary.harness_command,
        "integration_level": summary.integration_level,
        "container_mode": summary.container_mode,
        "image": summary.image,
        "network": summary.network_mode,
        "events_path": summary.events_path,
        "tracing": summary.tracing_mode,
        "control_path": summary.control_path,
        "harness_script_resolved": summary.harness_script_resolved.as_ref().map(|p| p.display().to_string()),
        "harness_script_exists": summary.harness_script_exists
    })
}

fn print_summary(summary: &lab_runner::ExperimentSummary) {
    println!("experiment: {}", summary.exp_id);
    println!("workload_type: {}", summary.workload_type);
    println!("dataset: {}", summary.dataset_path.display());
    println!("tasks: {}", summary.task_count);
    println!("replications: {}", summary.replications);
    println!("variant_plan_entries: {}", summary.variant_count);
    println!("total_trials: {}", summary.total_trials);
    println!("harness: {:?}", summary.harness_command);
    println!("integration_level: {}", summary.integration_level);
    println!("container_mode: {}", summary.container_mode);
    if let Some(image) = &summary.image {
        println!("image: {}", image);
    }
    println!("network: {}", summary.network_mode);
    if let Some(events) = &summary.events_path {
        println!("events_path: {}", events);
    }
    if let Some(mode) = &summary.tracing_mode {
        println!("tracing: {}", mode);
    }
    println!("control_path: {}", summary.control_path);
    if let Some(p) = &summary.harness_script_resolved {
        println!("harness_script_resolved: {}", p.display());
        println!("harness_script_exists: {}", summary.harness_script_exists);
    }
}

fn write_knob_files(
    manifest: &std::path::Path,
    overrides: &std::path::Path,
    force: bool,
) -> Result<()> {
    if let Some(parent) = manifest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = overrides.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if force || !manifest.exists() {
        let manifest_template = r#"{
  "schema_version": "knob_manifest_v1",
  "knobs": [
    {
      "id": "design.replications",
      "label": "Replications",
      "json_pointer": "/design/replications",
      "type": "integer",
      "minimum": 1,
      "maximum": 100,
      "role": "core",
      "scientific_role": "control",
      "autotune": { "enabled": true, "requires_human_approval": false }
    },
    {
      "id": "dataset.limit",
      "label": "Dataset Limit",
      "json_pointer": "/dataset/limit",
      "type": "integer",
      "minimum": 1,
      "role": "core",
      "scientific_role": "control"
    },
    {
      "id": "runtime.network.mode",
      "label": "Network Mode",
      "json_pointer": "/runtime/network/mode",
      "type": "string",
      "options": ["none", "full", "allowlist_enforced"],
      "role": "infra",
      "scientific_role": "invariant"
    },
    {
      "id": "runtime.harness.integration_level",
      "label": "Integration Level",
      "json_pointer": "/runtime/harness/integration_level",
      "type": "string",
      "options": ["cli_basic", "cli_events", "otel", "sdk_control", "sdk_full"],
      "role": "harness",
      "scientific_role": "confound"
    },
    {
      "id": "runtime.harness.command",
      "label": "Harness Command",
      "json_pointer": "/runtime/harness/command",
      "type": "array",
      "role": "harness",
      "scientific_role": "treatment",
      "autotune": { "enabled": false, "requires_human_approval": true }
    }
  ]
}
"#;
        std::fs::write(manifest, manifest_template)?;
    }

    if force || !overrides.exists() {
        let manifest_rel = if manifest.is_absolute() {
            if let Ok(cwd) = std::env::current_dir() {
                if let Ok(rel) = manifest.strip_prefix(&cwd) {
                    rel.to_string_lossy().to_string()
                } else {
                    manifest.display().to_string()
                }
            } else {
                manifest.display().to_string()
            }
        } else {
            manifest.to_string_lossy().to_string()
        };
        let overrides_template = format!(
            "{{\n  \"schema_version\": \"experiment_overrides_v1\",\n  \"manifest_path\": \"{}\",\n  \"values\": {{\n    \"design.replications\": 1\n  }}\n}}\n",
            manifest_rel.replace('\\', "\\\\")
        );
        std::fs::write(overrides, overrides_template)?;
    }

    Ok(())
}
