use anyhow::{anyhow, Result};
use chrono::Utc;
use lab_analysis::{summarize_trial, write_analysis};
use lab_core::{canonical_json_digest, ensure_dir, sha256_bytes, sha256_file, ArtifactStore};
use lab_hooks::{load_manifest, validate_hooks};
use lab_provenance::{default_attestation, write_attestation};
use lab_schemas::compile_schema;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub struct RunResult {
    pub run_dir: PathBuf,
    pub run_id: String,
}

pub struct ReplayResult {
    pub replay_dir: PathBuf,
    pub replay_id: String,
    pub parent_trial_id: String,
    pub strict: bool,
    pub replay_grade: String,
    pub harness_status: String,
}

pub struct ForkResult {
    pub fork_dir: PathBuf,
    pub fork_id: String,
    pub parent_trial_id: String,
    pub selector: String,
    pub strict: bool,
    pub replay_grade: String,
    pub harness_status: String,
    pub source_checkpoint: Option<String>,
    pub fallback_mode: String,
}

pub struct PauseResult {
    pub run_id: String,
    pub trial_id: String,
    pub label: String,
    pub checkpoint_acked: bool,
    pub stop_acked: bool,
}

pub struct ResumeResult {
    pub trial_id: String,
    pub selector: String,
    pub fork: ForkResult,
}

enum ForkSelector {
    Checkpoint(String),
    Step(u64),
    EventSeq(u64),
}

#[derive(Debug)]
struct RunOperationLock {
    path: PathBuf,
}

impl Drop for RunOperationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_run_operation_lock(run_dir: &Path) -> Result<RunOperationLock> {
    let lock_path = run_dir.join("runtime").join("operation.lock");
    if let Some(parent) = lock_path.parent() {
        ensure_dir(parent)?;
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut file) => {
            let payload = format!(
                "{{\"pid\":{},\"acquired_at\":\"{}\"}}\n",
                std::process::id(),
                Utc::now().to_rfc3339()
            );
            let _ = file.write_all(payload.as_bytes());
            let _ = file.sync_all();
            Ok(RunOperationLock { path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
            "operation_in_progress: run is already under control operation"
        )),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Deserialize)]
struct ExperimentOverrides {
    schema_version: String,
    #[serde(default)]
    manifest_path: Option<String>,
    #[serde(default)]
    values: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct KnobManifest {
    schema_version: String,
    knobs: Vec<KnobDef>,
}

#[derive(Debug, Deserialize)]
struct KnobDef {
    id: String,
    json_pointer: String,
    #[serde(rename = "type")]
    value_type: String,
    #[serde(default)]
    options: Option<Vec<Value>>,
    #[serde(default)]
    minimum: Option<f64>,
    #[serde(default)]
    maximum: Option<f64>,
}

pub fn validate_knob_overrides(manifest_path: &Path, overrides_path: &Path) -> Result<()> {
    let manifest = load_knob_manifest(manifest_path)?;
    let overrides = load_experiment_overrides(overrides_path)?;
    let mut by_id: BTreeMap<String, KnobDef> = BTreeMap::new();
    for knob in manifest.knobs {
        by_id.insert(knob.id.clone(), knob);
    }
    for (id, value) in overrides.values.iter() {
        let knob = by_id
            .get(id)
            .ok_or_else(|| anyhow!("override references unknown knob id: {}", id))?;
        validate_knob_value(knob, value)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct RunBehavior {
    pub setup_command: Option<String>,
    pub network_mode_override: Option<String>,
    pub require_network_none: bool,
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let ts = Utc::now().timestamp_micros();
    let pid = std::process::id();
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("tmpfile");
    let tmp = path.with_file_name(format!(".{}.tmp.{}.{}", name, pid, ts));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn atomic_write_json_pretty(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write_bytes(path, &bytes)
}

fn run_control_path(run_dir: &Path) -> PathBuf {
    run_dir.join("runtime").join("run_control.json")
}

fn write_run_control(
    run_dir: &Path,
    run_id: &str,
    status: &str,
    active_trial_id: Option<&str>,
    active_control_path: Option<&Path>,
) -> Result<()> {
    let payload = json!({
        "schema_version": "run_control_v1",
        "run_id": run_id,
        "status": status,
        "active_trial_id": active_trial_id,
        "active_control_path": active_control_path.map(|p| p.to_string_lossy().to_string()),
        "updated_at": Utc::now().to_rfc3339(),
    });
    atomic_write_json_pretty(&run_control_path(run_dir), &payload)
}

fn write_trial_state(
    trial_dir: &Path,
    trial_id: &str,
    status: &str,
    pause_label: Option<&str>,
    checkpoint_selected: Option<&str>,
    exit_reason: Option<&str>,
) -> Result<()> {
    let payload = json!({
        "schema_version": "trial_state_v1",
        "trial_id": trial_id,
        "status": status,
        "pause_label": pause_label,
        "checkpoint_selected": checkpoint_selected,
        "exit_reason": exit_reason,
        "updated_at": Utc::now().to_rfc3339(),
    });
    atomic_write_json_pretty(&trial_dir.join("trial_state.json"), &payload)
}

struct RunControlGuard {
    run_dir: PathBuf,
    run_id: String,
    done: bool,
}

impl RunControlGuard {
    fn new(run_dir: &Path, run_id: &str) -> Self {
        Self {
            run_dir: run_dir.to_path_buf(),
            run_id: run_id.to_string(),
            done: false,
        }
    }

    fn complete(&mut self, status: &str) -> Result<()> {
        write_run_control(&self.run_dir, &self.run_id, status, None, None)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for RunControlGuard {
    fn drop(&mut self) {
        if !self.done {
            let _ = write_run_control(&self.run_dir, &self.run_id, "failed", None, None);
        }
    }
}

struct TrialStateGuard {
    trial_dir: PathBuf,
    trial_id: String,
    done: bool,
}

impl TrialStateGuard {
    fn new(trial_dir: &Path, trial_id: &str) -> Self {
        Self {
            trial_dir: trial_dir.to_path_buf(),
            trial_id: trial_id.to_string(),
            done: false,
        }
    }

    fn complete(&mut self, status: &str, exit_reason: Option<&str>) -> Result<()> {
        write_trial_state(
            &self.trial_dir,
            &self.trial_id,
            status,
            None,
            None,
            exit_reason,
        )?;
        self.done = true;
        Ok(())
    }
}

impl Drop for TrialStateGuard {
    fn drop(&mut self) {
        if !self.done {
            let _ = write_trial_state(
                &self.trial_dir,
                &self.trial_id,
                "failed",
                None,
                None,
                Some("aborted"),
            );
        }
    }
}

pub fn find_project_root(experiment_dir: &Path) -> PathBuf {
    let mut cur = Some(experiment_dir);
    while let Some(p) = cur {
        if p.file_name().and_then(|s| s.to_str()) == Some(".lab") {
            return p.parent().unwrap_or(experiment_dir).to_path_buf();
        }
        cur = p.parent();
    }
    experiment_dir.to_path_buf()
}

#[derive(Debug, Clone)]
pub struct ExperimentSummary {
    pub exp_id: String,
    pub workload_type: String,
    pub dataset_path: PathBuf,
    pub task_count: usize,
    pub replications: usize,
    pub variant_count: usize,
    pub total_trials: usize,
    pub harness_command: Vec<String>,
    pub integration_level: String,
    pub container_mode: bool,
    pub image: Option<String>,
    pub network_mode: String,
    pub events_path: Option<String>,
    pub tracing_mode: Option<String>,
    pub control_path: String,
    pub harness_script_resolved: Option<PathBuf>,
    pub harness_script_exists: bool,
}

pub fn run_experiment(path: &Path, use_container: bool) -> Result<RunResult> {
    run_experiment_with_behavior(path, use_container, RunBehavior::default(), None)
}

pub fn run_experiment_dev(path: &Path, setup_command: Option<String>) -> Result<RunResult> {
    run_experiment_dev_with_overrides(path, setup_command, None)
}

pub fn run_experiment_with_overrides(
    path: &Path,
    use_container: bool,
    overrides_path: Option<&Path>,
) -> Result<RunResult> {
    run_experiment_with_behavior(path, use_container, RunBehavior::default(), overrides_path)
}

pub fn run_experiment_dev_with_overrides(
    path: &Path,
    setup_command: Option<String>,
    overrides_path: Option<&Path>,
) -> Result<RunResult> {
    let behavior = RunBehavior {
        setup_command,
        network_mode_override: Some("full".to_string()),
        require_network_none: false,
    };
    run_experiment_with_behavior(path, true, behavior, overrides_path)
}

pub fn run_experiment_strict(path: &Path) -> Result<RunResult> {
    run_experiment_strict_with_overrides(path, None)
}

pub fn run_experiment_strict_with_overrides(
    path: &Path,
    overrides_path: Option<&Path>,
) -> Result<RunResult> {
    let behavior = RunBehavior {
        setup_command: None,
        network_mode_override: None,
        require_network_none: true,
    };
    run_experiment_with_behavior(path, true, behavior, overrides_path)
}

pub fn replay_trial(run_dir: &Path, trial_id: &str, strict: bool) -> Result<ReplayResult> {
    let _op_lock = acquire_run_operation_lock(run_dir)?;
    let run_dir = run_dir
        .canonicalize()
        .map_err(|_| anyhow!("run_dir not found: {}", run_dir.display()))?;
    let project_root = find_project_root(&run_dir)
        .canonicalize()
        .unwrap_or_else(|_| find_project_root(&run_dir));

    let resolved_path = run_dir.join("resolved_experiment.json");
    if !resolved_path.exists() {
        return Err(anyhow!(
            "missing resolved_experiment.json in {}",
            run_dir.display()
        ));
    }
    let json_value: Value = serde_json::from_slice(&fs::read(&resolved_path)?)?;
    let harness = resolve_harness(&json_value, &project_root)?;
    validate_harness_command(&harness.command_raw, &project_root)?;

    if strict && harness.integration_level != "sdk_full" {
        return Err(anyhow!(
            "strict replay requires integration_level sdk_full (found: {})",
            harness.integration_level
        ));
    }

    let parent_trial_dir = run_dir.join("trials").join(trial_id);
    if !parent_trial_dir.exists() {
        return Err(anyhow!("parent trial not found: {}", trial_id));
    }
    let parent_input_path = parent_trial_dir.join("trial_input.json");
    if !parent_input_path.exists() {
        return Err(anyhow!(
            "parent trial missing trial_input.json: {}",
            parent_input_path.display()
        ));
    }
    let mut input: Value = serde_json::from_slice(&fs::read(&parent_input_path)?)?;

    let replay_id = format!("replay_{}", Utc::now().format("%Y%m%d_%H%M%S"));
    let replay_dir = run_dir.join("replays").join(&replay_id);
    ensure_dir(&replay_dir)?;

    let replay_trial_id = format!("{}_{}", trial_id, replay_id);
    set_json_pointer_value(
        &mut input,
        "/ids/trial_id",
        Value::String(replay_trial_id.clone()),
    )?;

    let dataset_src = first_file_in_dir(&parent_trial_dir.join("dataset"))?;
    let replay_trial_dir = replay_dir.join("trial_1");
    ensure_dir(&replay_trial_dir)?;
    write_trial_state(
        &replay_trial_dir,
        &replay_trial_id,
        "running",
        None,
        None,
        None,
    )?;
    let mut trial_guard = TrialStateGuard::new(&replay_trial_dir, &replay_trial_id);

    let workspace_src = if parent_trial_dir.join("workspace").exists() {
        parent_trial_dir.join("workspace")
    } else {
        project_root.clone()
    };
    let trial_paths = TrialPaths::new(&replay_trial_dir, &workspace_src, &dataset_src)?;
    trial_paths.prepare()?;

    let input_bytes = serde_json::to_vec_pretty(&input)?;
    let canonical_input = replay_trial_dir.join("trial_input.json");
    atomic_write_bytes(&canonical_input, &input_bytes)?;
    let container_mode = input
        .pointer("/runtime/paths/workspace")
        .and_then(|v| v.as_str())
        == Some("/workspace");
    let (input_path, output_path) = prepare_io_paths(&trial_paths, container_mode, &input_bytes)?;
    let (control_path_harness, control_path_host) =
        resolve_control_paths(&harness.control_path, &trial_paths, container_mode);
    write_control_file(&control_path_host)?;

    let effective_network_mode = input
        .pointer("/runtime/network/mode_requested")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let status = if container_mode {
        let command = resolve_command_container(&harness.command_raw, &project_root);
        run_harness_container(
            &json_value,
            &harness,
            &trial_paths,
            &input_path,
            &output_path,
            &control_path_harness,
            &command,
            &effective_network_mode,
            None,
        )?
    } else {
        let command = resolve_command_local(&harness.command_raw, &project_root);
        run_harness_local(
            &harness,
            &trial_paths,
            &input_path,
            &output_path,
            &control_path_harness,
            &command,
        )?
    };

    if container_mode {
        let canonical_output = replay_trial_dir.join("trial_output.json");
        if output_path.exists() {
            let output_bytes = fs::read(&output_path)?;
            atomic_write_bytes(&canonical_output, &output_bytes)?;
        }
    }

    let canonical_output = replay_trial_dir.join("trial_output.json");
    let trial_output: Value = if canonical_output.exists() {
        serde_json::from_slice(&fs::read(&canonical_output)?)?
    } else {
        json!({"schema_version":"trial_output_v1","outcome":"error"})
    };

    let outcome = trial_output
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("error");
    if status == "0" && outcome != "error" {
        trial_guard.complete("completed", None)?;
    } else if status != "0" {
        trial_guard.complete("failed", Some("harness_exit_nonzero"))?;
    } else {
        trial_guard.complete("failed", Some("trial_output_error"))?;
    }

    let replay_grade = replay_grade_for_integration(&harness.integration_level).to_string();
    let manifest = json!({
        "schema_version": "replay_manifest_v1",
        "operation": "replay",
        "replay_id": replay_id.clone(),
        "parent_trial_id": trial_id,
        "strict": strict,
        "integration_level": harness.integration_level.clone(),
        "replay_grade": replay_grade.clone(),
        "created_at": Utc::now().to_rfc3339(),
    });
    atomic_write_json_pretty(&replay_dir.join("manifest.json"), &manifest)?;

    Ok(ReplayResult {
        replay_dir,
        replay_id,
        parent_trial_id: trial_id.to_string(),
        strict,
        replay_grade,
        harness_status: status,
    })
}

fn first_file_in_dir(dir: &Path) -> Result<PathBuf> {
    if !dir.exists() {
        return Err(anyhow!("directory not found: {}", dir.display()));
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("no files found in {}", dir.display()))
}

fn replay_grade_for_integration(level: &str) -> &'static str {
    match level {
        "sdk_full" => "strict",
        "sdk_control" => "checkpointed",
        "cli_events" | "otel" => "best_effort",
        _ => "best_effort",
    }
}

pub fn fork_trial(
    run_dir: &Path,
    from_trial: &str,
    selector: &str,
    set_bindings: &BTreeMap<String, Value>,
    strict: bool,
) -> Result<ForkResult> {
    let _op_lock = acquire_run_operation_lock(run_dir)?;
    fork_trial_inner(run_dir, from_trial, selector, set_bindings, strict)
}

fn fork_trial_inner(
    run_dir: &Path,
    from_trial: &str,
    selector: &str,
    set_bindings: &BTreeMap<String, Value>,
    strict: bool,
) -> Result<ForkResult> {
    let run_dir = run_dir
        .canonicalize()
        .map_err(|_| anyhow!("run_dir not found: {}", run_dir.display()))?;
    let project_root = find_project_root(&run_dir)
        .canonicalize()
        .unwrap_or_else(|_| find_project_root(&run_dir));

    let resolved_path = run_dir.join("resolved_experiment.json");
    if !resolved_path.exists() {
        return Err(anyhow!(
            "missing resolved_experiment.json in {}",
            run_dir.display()
        ));
    }
    let json_value: Value = serde_json::from_slice(&fs::read(&resolved_path)?)?;
    let harness = resolve_harness(&json_value, &project_root)?;
    validate_harness_command(&harness.command_raw, &project_root)?;

    if strict && harness.integration_level != "sdk_full" {
        return Err(anyhow!(
            "strict fork requires integration_level sdk_full (found: {})",
            harness.integration_level
        ));
    }

    let parent_trial_dir = run_dir.join("trials").join(from_trial);
    if !parent_trial_dir.exists() {
        return Err(anyhow!("parent trial not found: {}", from_trial));
    }
    let parent_input_path = parent_trial_dir.join("trial_input.json");
    if !parent_input_path.exists() {
        return Err(anyhow!(
            "parent trial missing trial_input.json: {}",
            parent_input_path.display()
        ));
    }
    let parent_output_path = parent_trial_dir.join("trial_output.json");
    let parent_output = if parent_output_path.exists() {
        Some(serde_json::from_slice::<Value>(&fs::read(
            &parent_output_path,
        )?)?)
    } else {
        None
    };
    let parsed_selector = parse_fork_selector(selector)?;
    let source_checkpoint = resolve_selector_checkpoint(
        &parsed_selector,
        parent_output.as_ref(),
        &parent_trial_dir,
        strict,
    )?;
    if strict && source_checkpoint.is_none() {
        return Err(anyhow!(
            "strict_source_unavailable: selector {} did not resolve to a committed checkpoint",
            selector
        ));
    }

    let run_id = run_dir
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("run")
        .to_string();

    let mut input: Value = serde_json::from_slice(&fs::read(&parent_input_path)?)?;
    let fork_id = format!("fork_{}", Utc::now().format("%Y%m%d_%H%M%S"));
    let fork_dir = run_dir.join("forks").join(&fork_id);
    ensure_dir(&fork_dir)?;
    let fork_trial_id = format!("{}_{}", from_trial, fork_id);
    set_json_pointer_value(
        &mut input,
        "/ids/trial_id",
        Value::String(fork_trial_id.clone()),
    )?;
    apply_binding_overrides(&mut input, set_bindings)?;
    set_json_pointer_value(
        &mut input,
        "/ext/fork",
        json!({
            "parent_run_id": run_id,
            "parent_trial_id": from_trial,
            "selector": selector,
            "source_checkpoint": source_checkpoint.clone(),
            "strict": strict
        }),
    )?;

    let dataset_src = first_file_in_dir(&parent_trial_dir.join("dataset"))?;
    let fork_trial_dir = fork_dir.join("trial_1");
    ensure_dir(&fork_trial_dir)?;
    write_trial_state(
        &fork_trial_dir,
        &fork_trial_id,
        "running",
        None,
        source_checkpoint.as_deref(),
        None,
    )?;
    let mut trial_guard = TrialStateGuard::new(&fork_trial_dir, &fork_trial_id);

    let workspace_src = if let Some(ref checkpoint) = source_checkpoint {
        let p = PathBuf::from(checkpoint);
        if p.is_dir() {
            p
        } else if parent_trial_dir.join("workspace").exists() {
            parent_trial_dir.join("workspace")
        } else {
            project_root.clone()
        }
    } else if parent_trial_dir.join("workspace").exists() {
        parent_trial_dir.join("workspace")
    } else {
        project_root.clone()
    };
    let trial_paths = TrialPaths::new(&fork_trial_dir, &workspace_src, &dataset_src)?;
    trial_paths.prepare()?;

    let input_bytes = serde_json::to_vec_pretty(&input)?;
    let canonical_input = fork_trial_dir.join("trial_input.json");
    atomic_write_bytes(&canonical_input, &input_bytes)?;
    let container_mode = input
        .pointer("/runtime/paths/workspace")
        .and_then(|v| v.as_str())
        == Some("/workspace");
    let (input_path, output_path) = prepare_io_paths(&trial_paths, container_mode, &input_bytes)?;
    let (control_path_harness, control_path_host) =
        resolve_control_paths(&harness.control_path, &trial_paths, container_mode);
    write_control_file(&control_path_host)?;

    let effective_network_mode = input
        .pointer("/runtime/network/mode_requested")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let status = if container_mode {
        let command = resolve_command_container(&harness.command_raw, &project_root);
        run_harness_container(
            &json_value,
            &harness,
            &trial_paths,
            &input_path,
            &output_path,
            &control_path_harness,
            &command,
            &effective_network_mode,
            None,
        )?
    } else {
        let command = resolve_command_local(&harness.command_raw, &project_root);
        run_harness_local(
            &harness,
            &trial_paths,
            &input_path,
            &output_path,
            &control_path_harness,
            &command,
        )?
    };

    if container_mode {
        let canonical_output = fork_trial_dir.join("trial_output.json");
        if output_path.exists() {
            let output_bytes = fs::read(&output_path)?;
            atomic_write_bytes(&canonical_output, &output_bytes)?;
        }
    }

    let canonical_output = fork_trial_dir.join("trial_output.json");
    let trial_output: Value = if canonical_output.exists() {
        serde_json::from_slice(&fs::read(&canonical_output)?)?
    } else {
        json!({"schema_version":"trial_output_v1","outcome":"error"})
    };
    let outcome = trial_output
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("error");
    if status == "0" && outcome != "error" {
        trial_guard.complete("completed", None)?;
    } else if status != "0" {
        trial_guard.complete("failed", Some("harness_exit_nonzero"))?;
    } else {
        trial_guard.complete("failed", Some("trial_output_error"))?;
    }

    let replay_grade = replay_grade_for_integration(&harness.integration_level).to_string();
    let fallback_mode = if source_checkpoint.is_some() {
        "checkpoint".to_string()
    } else {
        "input_only".to_string()
    };
    let manifest = json!({
        "schema_version": "fork_manifest_v1",
        "operation": "fork",
        "fork_id": fork_id.clone(),
        "parent_trial_id": from_trial,
        "selector": selector,
        "source_checkpoint": source_checkpoint.clone(),
        "fallback_mode": fallback_mode.clone(),
        "strict": strict,
        "integration_level": harness.integration_level.clone(),
        "replay_grade": replay_grade.clone(),
        "created_at": Utc::now().to_rfc3339(),
    });
    atomic_write_json_pretty(&fork_dir.join("manifest.json"), &manifest)?;

    Ok(ForkResult {
        fork_dir,
        fork_id,
        parent_trial_id: from_trial.to_string(),
        selector: selector.to_string(),
        strict,
        replay_grade,
        harness_status: status,
        source_checkpoint,
        fallback_mode,
    })
}

pub fn pause_run(
    run_dir: &Path,
    trial_id: Option<&str>,
    label: Option<&str>,
    timeout_seconds: u64,
) -> Result<PauseResult> {
    let _op_lock = acquire_run_operation_lock(run_dir)?;
    let run_dir = run_dir
        .canonicalize()
        .map_err(|_| anyhow!("run_dir not found: {}", run_dir.display()))?;
    let run_control = load_json_file(&run_control_path(&run_dir))?;
    let status = run_control
        .pointer("/status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if status != "running" {
        return Err(anyhow!("pause_non_running: run status is {}", status));
    }

    let run_id = run_control
        .pointer("/run_id")
        .and_then(|v| v.as_str())
        .unwrap_or("run")
        .to_string();
    let active_trial = run_control
        .pointer("/active_trial_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let target_trial = if let Some(id) = trial_id {
        if let Some(active) = active_trial.as_ref() {
            if active != id {
                return Err(anyhow!(
                    "pause_target_not_active: active trial is {}, requested {}",
                    active,
                    id
                ));
            }
        }
        id.to_string()
    } else {
        active_trial.ok_or_else(|| anyhow!("pause_no_active_trial"))?
    };
    let control_path = run_control
        .pointer("/active_control_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("pause_missing_control_path"))?;

    let resolved = load_json_file(&run_dir.join("resolved_experiment.json"))?;
    let integration_level = resolved
        .pointer("/runtime/harness/integration_level")
        .and_then(|v| v.as_str())
        .unwrap_or("cli_basic");
    if integration_level == "cli_basic" {
        return Err(anyhow!(
            "unsupported_for_integration_level: pause requires cli_events or higher"
        ));
    }
    let events_path_cfg = resolved
        .pointer("/runtime/harness/events/path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("pause_requires_events_path"))?;

    let trial_dir = run_dir.join("trials").join(&target_trial);
    if !trial_dir.exists() {
        return Err(anyhow!("pause_trial_not_found: {}", target_trial));
    }
    let container_mode = trial_is_container_mode(&trial_dir)?;
    let events_path = resolve_event_path_for_trial(events_path_cfg, &trial_dir, container_mode);

    let pause_label = label.unwrap_or("pause").to_string();
    let timeout = Duration::from_secs(timeout_seconds.max(1));
    let deadline = Instant::now() + timeout;

    let seq_checkpoint = read_control_seq(&control_path)? + 1;
    let checkpoint_version = write_control_action(
        &control_path,
        seq_checkpoint,
        "checkpoint",
        Some(&pause_label),
        "lab_pause",
    )?;
    wait_for_control_ack(&events_path, "checkpoint", &checkpoint_version, deadline)?;

    let seq_stop = read_control_seq(&control_path)? + 1;
    let stop_version = write_control_action(
        &control_path,
        seq_stop,
        "stop",
        Some(&pause_label),
        "lab_pause",
    )?;
    wait_for_control_ack(&events_path, "stop", &stop_version, deadline)?;

    write_trial_state(
        &trial_dir,
        &target_trial,
        "paused",
        Some(&pause_label),
        Some(&pause_label),
        Some("paused_by_user"),
    )?;
    write_run_control(
        &run_dir,
        &run_id,
        "paused",
        Some(&target_trial),
        Some(&control_path),
    )?;

    Ok(PauseResult {
        run_id,
        trial_id: target_trial,
        label: pause_label,
        checkpoint_acked: true,
        stop_acked: true,
    })
}

pub fn resume_run(
    run_dir: &Path,
    trial_id: Option<&str>,
    label: Option<&str>,
    set_bindings: &BTreeMap<String, Value>,
    strict: bool,
) -> Result<ResumeResult> {
    let _op_lock = acquire_run_operation_lock(run_dir)?;
    let run_dir = run_dir
        .canonicalize()
        .map_err(|_| anyhow!("run_dir not found: {}", run_dir.display()))?;
    let run_control = load_json_file(&run_control_path(&run_dir))?;
    let status = run_control
        .pointer("/status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if status != "paused" {
        return Err(anyhow!("resume_non_paused: run status is {}", status));
    }

    let active_trial = run_control
        .pointer("/active_trial_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let target_trial = if let Some(id) = trial_id {
        id.to_string()
    } else {
        active_trial.ok_or_else(|| anyhow!("resume_no_active_trial"))?
    };
    let trial_dir = run_dir.join("trials").join(&target_trial);
    if !trial_dir.exists() {
        return Err(anyhow!("resume_trial_not_found: {}", target_trial));
    }
    let trial_state_path = trial_dir.join("trial_state.json");
    if !trial_state_path.exists() {
        return Err(anyhow!(
            "resume_missing_trial_state: {}",
            trial_state_path.display()
        ));
    }
    let trial_state = load_json_file(&trial_state_path)?;
    let trial_status = trial_state
        .pointer("/status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if trial_status != "paused" {
        return Err(anyhow!(
            "resume_trial_not_paused: trial {} status is {}",
            target_trial,
            trial_status
        ));
    }
    let pause_label = trial_state.pointer("/pause_label").and_then(|v| v.as_str());
    let selector = resolve_resume_selector(&trial_dir, label.or(pause_label))?;

    let fork = fork_trial_inner(&run_dir, &target_trial, &selector, set_bindings, strict)?;
    Ok(ResumeResult {
        trial_id: target_trial,
        selector,
        fork,
    })
}

fn load_json_file(path: &Path) -> Result<Value> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn resolve_resume_selector(trial_dir: &Path, preferred_label: Option<&str>) -> Result<String> {
    let output_path = trial_dir.join("trial_output.json");
    if !output_path.exists() {
        return Err(anyhow!("resume_no_trial_output: {}", output_path.display()));
    }
    let output = load_json_file(&output_path)?;
    let checkpoints = output
        .get("checkpoints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if checkpoints.is_empty() {
        return Err(anyhow!(
            "resume_no_checkpoint: paused trial has no declared checkpoints"
        ));
    }

    if let Some(label) = preferred_label {
        let found = checkpoints.iter().any(|cp| {
            cp.get("logical_name").and_then(|v| v.as_str()) == Some(label)
                || cp.get("path").and_then(|v| v.as_str()) == Some(label)
        });
        if !found {
            return Err(anyhow!(
                "resume_checkpoint_not_found: label '{}' was not found in trial checkpoints",
                label
            ));
        }
        return Ok(format!("checkpoint:{}", label));
    }

    let mut best_with_step: Option<(u64, Value)> = None;
    for cp in checkpoints.iter() {
        if let Some(step) = cp.get("step").and_then(|v| v.as_u64()) {
            match best_with_step {
                Some((cur, _)) if step <= cur => {}
                _ => best_with_step = Some((step, cp.clone())),
            }
        }
    }
    let chosen = if let Some((_, cp)) = best_with_step {
        cp
    } else {
        checkpoints
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("resume_no_checkpoint"))?
    };
    if let Some(name) = chosen.get("logical_name").and_then(|v| v.as_str()) {
        return Ok(format!("checkpoint:{}", name));
    }
    if let Some(path) = chosen.get("path").and_then(|v| v.as_str()) {
        return Ok(format!("checkpoint:{}", path));
    }
    Err(anyhow!("resume_no_checkpoint_token"))
}

fn trial_is_container_mode(trial_dir: &Path) -> Result<bool> {
    let input = load_json_file(&trial_dir.join("trial_input.json"))?;
    Ok(input
        .pointer("/runtime/paths/workspace")
        .and_then(|v| v.as_str())
        == Some("/workspace"))
}

fn resolve_event_path_for_trial(
    events_path: &str,
    trial_dir: &Path,
    _container_mode: bool,
) -> PathBuf {
    if let Some(rest) = events_path.strip_prefix("/state") {
        return trial_dir.join("state").join(rest.trim_start_matches('/'));
    }
    if let Some(rest) = events_path.strip_prefix("/out") {
        return trial_dir.join("out").join(rest.trim_start_matches('/'));
    }
    if let Some(rest) = events_path.strip_prefix("/workspace") {
        return trial_dir
            .join("workspace")
            .join(rest.trim_start_matches('/'));
    }
    if let Some(rest) = events_path.strip_prefix("/dataset") {
        return trial_dir.join("dataset").join(rest.trim_start_matches('/'));
    }
    if let Some(rest) = events_path.strip_prefix("/tmp") {
        return trial_dir.join("tmp").join(rest.trim_start_matches('/'));
    }
    let p = Path::new(events_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        trial_dir.join("workspace").join(p)
    }
}

fn read_control_seq(control_path: &Path) -> Result<u64> {
    if !control_path.exists() {
        return Ok(0);
    }
    let value = load_json_file(control_path)?;
    Ok(value.pointer("/seq").and_then(|v| v.as_u64()).unwrap_or(0))
}

fn read_control_action(control_path: &Path) -> Result<Option<(String, String, Option<String>)>> {
    if !control_path.exists() {
        return Ok(None);
    }
    let value = load_json_file(control_path)?;
    let action = value
        .pointer("/action")
        .and_then(|v| v.as_str())
        .unwrap_or("continue")
        .to_string();
    let requested_by = value
        .pointer("/requested_by")
        .and_then(|v| v.as_str())
        .unwrap_or("run_loop")
        .to_string();
    let label = value
        .pointer("/label")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(Some((action, requested_by, label)))
}

fn wait_for_control_ack(
    events_path: &Path,
    action: &str,
    control_version: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        if has_control_ack(events_path, action, control_version)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "control_ack_missing: action={}, control_version={}, events_path={}",
                action,
                control_version,
                events_path.display()
            ));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn has_control_ack(events_path: &Path, action: &str, control_version: &str) -> Result<bool> {
    if !events_path.exists() {
        return Ok(false);
    }
    let data = fs::read_to_string(events_path)?;
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if parsed.get("event_type").and_then(|v| v.as_str()) != Some("control_ack") {
            continue;
        }
        if parsed
            .get("action_observed")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            != action
        {
            continue;
        }
        if parsed
            .get("control_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            == control_version
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_fork_selector(selector: &str) -> Result<ForkSelector> {
    let (kind, value) = selector
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid selector '{}': expected kind:value", selector))?;
    match kind {
        "checkpoint" => {
            if value.trim().is_empty() {
                return Err(anyhow!(
                    "invalid selector '{}': checkpoint name empty",
                    selector
                ));
            }
            Ok(ForkSelector::Checkpoint(value.to_string()))
        }
        "step" => Ok(ForkSelector::Step(value.parse::<u64>().map_err(|_| {
            anyhow!("invalid selector '{}': step must be integer", selector)
        })?)),
        "event_seq" => Ok(ForkSelector::EventSeq(value.parse::<u64>().map_err(
            |_| anyhow!("invalid selector '{}': event_seq must be integer", selector),
        )?)),
        _ => Err(anyhow!(
            "invalid selector kind '{}': expected checkpoint|step|event_seq",
            kind
        )),
    }
}

fn resolve_selector_checkpoint(
    selector: &ForkSelector,
    trial_output: Option<&Value>,
    trial_dir: &Path,
    strict: bool,
) -> Result<Option<String>> {
    let checkpoints = trial_output
        .and_then(|v| v.get("checkpoints"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let selected = match selector {
        ForkSelector::Checkpoint(name) => checkpoints.into_iter().find(|cp| {
            cp.get("logical_name").and_then(|v| v.as_str()) == Some(name.as_str())
                || cp.get("path").and_then(|v| v.as_str()) == Some(name.as_str())
        }),
        ForkSelector::Step(step) => checkpoints
            .into_iter()
            .filter_map(|cp| {
                let cp_step = cp.get("step").and_then(|v| v.as_u64());
                cp_step.map(|s| (s, cp))
            })
            .filter(|(s, _)| *s <= *step)
            .max_by_key(|(s, _)| *s)
            .map(|(_, cp)| cp),
        ForkSelector::EventSeq(seq) => checkpoints
            .into_iter()
            .filter_map(|cp| {
                let cp_step = cp.get("step").and_then(|v| v.as_u64());
                cp_step.map(|s| (s, cp))
            })
            .filter(|(s, _)| *s <= *seq)
            .max_by_key(|(s, _)| *s)
            .map(|(_, cp)| cp),
    };

    let Some(cp) = selected else {
        if strict {
            return Err(anyhow!(
                "strict_source_unavailable: selector checkpoint not found"
            ));
        }
        return Ok(None);
    };

    let raw_path = cp
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("invalid checkpoint entry: missing path"))?;
    let resolved = resolve_event_path_for_trial(raw_path, trial_dir, true);
    if strict && !resolved.exists() {
        return Err(anyhow!(
            "strict_source_unavailable: checkpoint path not found {}",
            resolved.display()
        ));
    }
    if resolved.exists() {
        Ok(Some(resolved.to_string_lossy().to_string()))
    } else {
        Ok(None)
    }
}

fn apply_binding_overrides(
    input: &mut Value,
    set_bindings: &BTreeMap<String, Value>,
) -> Result<()> {
    if set_bindings.is_empty() {
        return Ok(());
    }
    if input.pointer("/bindings").is_none() {
        set_json_pointer_value(input, "/bindings", json!({}))?;
    }
    for (key, value) in set_bindings {
        let pointer = format!("/bindings/{}", key.split('.').collect::<Vec<_>>().join("/"));
        set_json_pointer_value(input, &pointer, value.clone())?;
    }
    Ok(())
}

fn validate_required_fields(json_value: &Value) -> Result<()> {
    let required: &[&str] = &[
        "/experiment/workload_type",
        "/design/sanitization_profile",
        "/design/replications",
        "/runtime/harness/command",
        "/runtime/harness/integration_level",
        "/runtime/harness/input_path",
        "/runtime/harness/output_path",
        "/runtime/harness/control_plane/path",
        "/runtime/network/mode",
        "/baseline/variant_id",
    ];
    let mut missing = Vec::new();
    for pointer in required {
        let value = json_value.pointer(pointer);
        let is_missing = match value {
            None => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Number(n)) => n.as_u64() == Some(0) && *pointer == "/design/replications",
            Some(Value::Array(a)) => a.is_empty() && *pointer == "/runtime/harness/command",
            _ => false,
        };
        if is_missing {
            missing.push(*pointer);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "experiment.yaml missing required fields:\n{}",
            missing
                .iter()
                .map(|p| format!("  - {}", p))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn run_experiment_with_behavior(
    path: &Path,
    use_container: bool,
    behavior: RunBehavior,
    overrides_path: Option<&Path>,
) -> Result<RunResult> {
    let exp_dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    let project_root = find_project_root(&exp_dir)
        .canonicalize()
        .unwrap_or_else(|_| find_project_root(&exp_dir));
    let raw_yaml = fs::read_to_string(path)?;
    let yaml_value: serde_yaml::Value = serde_yaml::from_str(&raw_yaml)?;
    let mut json_value: Value = serde_json::to_value(yaml_value)?;
    if let Some(overrides_path) = overrides_path {
        json_value = apply_experiment_overrides(json_value, overrides_path, &project_root)?;
    }
    validate_required_fields(&json_value)?;
    let workload_type = json_value
        .pointer("/experiment/workload_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /experiment/workload_type"))?
        .to_string();
    let configured_network_mode = json_value
        .pointer("/runtime/network/mode")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /runtime/network/mode"))?;
    let effective_network_mode = behavior
        .network_mode_override
        .as_deref()
        .unwrap_or(configured_network_mode)
        .to_string();
    if behavior.require_network_none && effective_network_mode != "none" {
        return Err(anyhow!(
            "run-experiment requires network mode 'none' (current effective mode: {})",
            effective_network_mode
        ));
    }

    let run_id = format!("run_{}", Utc::now().format("%Y%m%d_%H%M%S"));
    let run_dir = project_root.join(".lab").join("runs").join(&run_id);
    ensure_dir(&run_dir)?;
    write_run_control(&run_dir, &run_id, "running", None, None)?;
    let mut run_guard = RunControlGuard::new(&run_dir, &run_id);

    let resolved_path = run_dir.join("resolved_experiment.json");
    atomic_write_json_pretty(&resolved_path, &json_value)?;
    let resolved_digest = canonical_json_digest(&json_value);
    atomic_write_bytes(
        &run_dir.join("resolved_experiment.digest"),
        resolved_digest.as_bytes(),
    )?;

    let manifest = json!({
        "schema_version": "manifest_v1",
        "run_id": run_id,
        "runner_version": "rust-0.3.0",
        "created_at": Utc::now().to_rfc3339(),
    });
    atomic_write_json_pretty(&run_dir.join("manifest.json"), &manifest)?;

    let dataset_path = resolve_dataset_path(&json_value, &exp_dir)?;
    let tasks = load_tasks(&dataset_path, &json_value)?;

    let (variants, baseline_id) = resolve_variant_plan(&json_value)?;
    let replications = json_value
        .pointer("/design/replications")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing /design/replications"))? as usize;

    let trials_dir = run_dir.join("trials");
    ensure_dir(&trials_dir)?;

    let analysis_dir = run_dir.join("analysis");
    ensure_dir(&analysis_dir)?;

    let _artifact_store = ArtifactStore::new(run_dir.join("trials").join("artifacts"));

    let harness = resolve_harness(&json_value, &project_root)?;
    validate_harness_command(&harness.command_raw, &project_root)?;
    let container_mode = use_container || harness.force_container;

    let mut trial_summaries = Vec::new();
    let mut event_counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut trial_event_counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();

    let mut trial_index: usize = 0;
    let mut run_paused = false;
    'variants: for variant in variants {
        for (task_idx, task) in tasks.iter().enumerate() {
            for repl in 0..replications {
                trial_index += 1;
                let trial_id = format!("trial_{}", trial_index);
                let trial_dir = trials_dir.join(&trial_id);
                ensure_dir(&trial_dir)?;
                write_trial_state(&trial_dir, &trial_id, "running", None, None, None)?;
                let mut trial_guard = TrialStateGuard::new(&trial_dir, &trial_id);

                let trial_paths = TrialPaths::new(&trial_dir, &project_root, &dataset_path)?;
                trial_paths.prepare()?;

                let input = build_trial_input(
                    &json_value,
                    &workload_type,
                    &trial_id,
                    &variant,
                    task_idx,
                    repl,
                    task,
                    &trial_paths,
                    container_mode,
                );
                let input_bytes = serde_json::to_vec_pretty(&input)?;
                let canonical_input_path = trial_dir.join("trial_input.json");
                atomic_write_bytes(&canonical_input_path, &input_bytes)?;

                let (input_path, output_path) =
                    prepare_io_paths(&trial_paths, container_mode, &input_bytes)?;

                let (control_path_harness, control_path_host) =
                    resolve_control_paths(&harness.control_path, &trial_paths, container_mode);
                write_run_control(
                    &run_dir,
                    &run_id,
                    "running",
                    Some(&trial_id),
                    Some(&control_path_host),
                )?;
                write_control_file(&control_path_host)?;

                let mut otel_receiver = None;
                let mut otel_manifest = None;
                if harness.tracing_mode == Some("otlp".to_string()) {
                    if container_mode
                        && json_value
                            .pointer("/runtime/network/mode")
                            .and_then(|v| v.as_str())
                            == Some("none")
                    {
                        otel_manifest = Some(json!({
                            "schema_version": "trace_manifest_v1",
                            "mode": "none",
                            "reason": "network_none",
                        }));
                    } else {
                        let receiver = lab_otel::OtlpReceiver::start(
                            4318,
                            ArtifactStore::new(trial_dir.join("artifacts")),
                        )?;
                        let endpoint = receiver.endpoint.clone();
                        otel_receiver = Some(receiver);
                        otel_manifest = Some(json!({
                            "schema_version": "trace_manifest_v1",
                            "mode": "otlp",
                            "endpoint": endpoint,
                        }));
                    }
                }

                let status = if container_mode {
                    let command = resolve_command_container(&harness.command_raw, &project_root);
                    run_harness_container(
                        &json_value,
                        &harness,
                        &trial_paths,
                        &input_path,
                        &output_path,
                        &control_path_harness,
                        &command,
                        &effective_network_mode,
                        behavior.setup_command.as_deref(),
                    )?
                } else {
                    if behavior.setup_command.is_some() {
                        return Err(anyhow!(
                            "setup command is only supported for container runs"
                        ));
                    }
                    let command = resolve_command_local(&harness.command_raw, &project_root);
                    run_harness_local(
                        &harness,
                        &trial_paths,
                        &input_path,
                        &output_path,
                        &control_path_harness,
                        &command,
                    )?
                };

                if let Some(receiver) = otel_receiver {
                    let records = receiver.records();
                    receiver.stop();
                    if let Some(mut manifest) = otel_manifest {
                        if let Some(obj) = manifest.as_object_mut() {
                            obj.insert("records".to_string(), serde_json::to_value(records)?);
                        }
                        let path = trial_dir.join("trace_manifest.json");
                        atomic_write_json_pretty(&path, &manifest)?;
                    }
                }

                if container_mode {
                    let canonical_output = trial_dir.join("trial_output.json");
                    if output_path.exists() {
                        let output_bytes = fs::read(&output_path)?;
                        atomic_write_bytes(&canonical_output, &output_bytes)?;
                    }
                }

                let canonical_output = trial_dir.join("trial_output.json");
                let trial_output: Value = if canonical_output.exists() {
                    serde_json::from_slice(&fs::read(&canonical_output)?)?
                } else {
                    json!({"schema_version": "trial_output_v1", "outcome": "error"})
                };

                let task_id = task
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("task_{}", task_idx));
                let summary = summarize_trial(
                    &run_id,
                    &trial_output,
                    &trial_id,
                    &workload_type,
                    &variant.id,
                    task_idx,
                    &task_id,
                    repl,
                    status.clone(),
                    container_mode,
                    &harness.integration_level,
                    configured_network_mode,
                    &effective_network_mode,
                );
                trial_summaries.push(summary);

                write_state_inventory(
                    &trial_dir,
                    &json_value,
                    &harness,
                    container_mode,
                    &trial_paths,
                    &resolve_exec_digest(&harness.command_raw, &project_root)?,
                    &effective_network_mode,
                )?;

                if let Some(events_path) = harness.events_path.as_ref() {
                    let manifest_path = resolve_harness_manifest_path(&trial_paths, container_mode);
                    if manifest_path.exists() {
                        let manifest = load_manifest(&manifest_path)?;
                        let schema = compile_schema("hook_events_v1.jsonschema")?;
                        let ev_path = resolve_event_path(events_path, &trial_paths, container_mode);
                        if ev_path.exists() {
                            let _ = validate_hooks(&manifest, &ev_path, &schema);
                            let counts = count_event_types(&ev_path)?;
                            let trial_map = trial_event_counts.entry(trial_id.clone()).or_default();
                            for (k, v) in counts.into_iter() {
                                *trial_map.entry(k.clone()).or_default() += v;
                                *event_counts
                                    .entry(variant.id.clone())
                                    .or_default()
                                    .entry(k)
                                    .or_default() += v;
                            }
                        }
                    }
                }

                let control_state = read_control_action(&control_path_host)?;
                let pause_requested = control_state
                    .as_ref()
                    .map(|(action, requested_by, _)| {
                        action == "stop" && requested_by == "lab_pause"
                    })
                    .unwrap_or(false);
                let pause_label = control_state
                    .as_ref()
                    .and_then(|(_, _, label)| label.as_deref());
                let outcome = trial_output
                    .get("outcome")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error");
                if pause_requested {
                    write_trial_state(
                        &trial_dir,
                        &trial_id,
                        "paused",
                        pause_label,
                        pause_label,
                        Some("paused_by_user"),
                    )?;
                    trial_guard.done = true;
                    write_run_control(
                        &run_dir,
                        &run_id,
                        "paused",
                        Some(&trial_id),
                        Some(&control_path_host),
                    )?;
                    run_paused = true;
                    break 'variants;
                } else if status == "0" && outcome != "error" {
                    trial_guard.complete("completed", None)?;
                } else if status != "0" {
                    trial_guard.complete("failed", Some("harness_exit_nonzero"))?;
                } else {
                    trial_guard.complete("failed", Some("trial_output_error"))?;
                }
                write_run_control(&run_dir, &run_id, "running", None, None)?;
            }
        }
    }

    write_analysis(
        &analysis_dir,
        &trial_summaries,
        &baseline_id,
        &event_counts,
        &trial_event_counts,
    )?;

    let grades = json!({
        "schema_version": "grades_v1",
        "integration_level": json_value.pointer("/runtime/harness/integration_level").and_then(|v| v.as_str()).unwrap_or("cli_basic"),
        "replay_grade": "best_effort",
        "isolation_grade": if use_container {"bounded"} else {"leaky"},
        "comparability_grade": "unknown",
        "provenance_grade": "recorded",
        "privacy_grade": "unknown"
    });

    let att = default_attestation(
        &resolved_digest,
        None,
        grades.clone(),
        vec![],
        json!({"name": "unknown"}),
        "hooks",
    );
    write_attestation(&run_dir, att)?;
    if run_paused {
        run_guard.complete("paused")?;
    } else {
        run_guard.complete("completed")?;
    }

    Ok(RunResult { run_dir, run_id })
}

pub fn describe_experiment(path: &Path) -> Result<ExperimentSummary> {
    describe_experiment_with_overrides(path, None)
}

pub fn describe_experiment_with_overrides(
    path: &Path,
    overrides_path: Option<&Path>,
) -> Result<ExperimentSummary> {
    let exp_dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    let project_root = find_project_root(&exp_dir)
        .canonicalize()
        .unwrap_or_else(|_| find_project_root(&exp_dir));
    let raw_yaml = fs::read_to_string(path)?;
    let yaml_value: serde_yaml::Value = serde_yaml::from_str(&raw_yaml)?;
    let mut json_value: Value = serde_json::to_value(yaml_value)?;
    if let Some(overrides_path) = overrides_path {
        json_value = apply_experiment_overrides(json_value, overrides_path, &project_root)?;
    }
    validate_required_fields(&json_value)?;

    let dataset_path = resolve_dataset_path(&json_value, &exp_dir)?;
    let task_count = count_tasks(&dataset_path, &json_value)?;
    let (variants, _) = resolve_variant_plan(&json_value)?;
    let replications = json_value
        .pointer("/design/replications")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing /design/replications"))? as usize;
    let variant_count = variants.len();
    let total_trials = task_count * replications * variant_count;

    let harness = resolve_harness(&json_value, &project_root)?;
    let container_mode = json_value
        .pointer("/runtime/sandbox/mode")
        .and_then(|v| v.as_str())
        == Some("container");
    let image = json_value
        .pointer("/runtime/sandbox/image")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let network_mode = json_value
        .pointer("/runtime/network/mode")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /runtime/network/mode"))?
        .to_string();

    let exp_id = json_value
        .pointer("/experiment/id")
        .and_then(|v| v.as_str())
        .unwrap_or("exp")
        .to_string();
    let workload_type = json_value
        .pointer("/experiment/workload_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /experiment/workload_type"))?
        .to_string();

    let harness_script_resolved = resolve_command_script_path(&harness.command_raw, &project_root);
    let harness_script_exists = harness_script_resolved
        .as_ref()
        .map(|p| p.exists())
        .unwrap_or(true);
    Ok(ExperimentSummary {
        exp_id,
        workload_type,
        dataset_path,
        task_count,
        replications,
        variant_count,
        total_trials,
        harness_command: harness.command_raw,
        integration_level: harness.integration_level,
        container_mode,
        image,
        network_mode,
        events_path: harness.events_path,
        tracing_mode: harness.tracing_mode,
        control_path: harness.control_path,
        harness_script_resolved,
        harness_script_exists,
    })
}

#[derive(Clone)]
struct Variant {
    id: String,
    bindings: Value,
}

fn resolve_variant_plan(json_value: &Value) -> Result<(Vec<Variant>, String)> {
    let baseline = json_value
        .pointer("/baseline/variant_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /baseline/variant_id"))?
        .to_string();
    let baseline_bindings = json_value
        .pointer("/baseline/bindings")
        .cloned()
        .unwrap_or(json!({}));

    let mut variants = Vec::new();
    variants.push(Variant {
        id: baseline.clone(),
        bindings: baseline_bindings,
    });

    let variant_list = json_value
        .pointer("/variant_plan")
        .and_then(|v| v.as_array())
        .or_else(|| json_value.pointer("/variants").and_then(|v| v.as_array()));
    if let Some(list) = variant_list {
        for item in list {
            let id = item
                .get("variant_id")
                .and_then(|v| v.as_str())
                .unwrap_or("variant")
                .to_string();
            let bindings = item.get("bindings").cloned().unwrap_or(json!({}));
            variants.push(Variant { id, bindings });
        }
    }
    Ok((variants, baseline))
}

fn apply_experiment_overrides(
    mut experiment: Value,
    overrides_path: &Path,
    project_root: &Path,
) -> Result<Value> {
    let overrides = load_experiment_overrides(overrides_path)?;
    if overrides.values.is_empty() {
        return Ok(experiment);
    }

    let manifest_rel = overrides
        .manifest_path
        .clone()
        .unwrap_or_else(|| ".lab/knobs/manifest.json".to_string());
    let manifest_path = if Path::new(&manifest_rel).is_absolute() {
        PathBuf::from(&manifest_rel)
    } else {
        project_root.join(&manifest_rel)
    };
    let manifest = load_knob_manifest(&manifest_path)?;

    let mut by_id: BTreeMap<String, KnobDef> = BTreeMap::new();
    for knob in manifest.knobs {
        by_id.insert(knob.id.clone(), knob);
    }

    for (id, value) in overrides.values.iter() {
        let knob = by_id
            .get(id)
            .ok_or_else(|| anyhow!("override references unknown knob id: {}", id))?;
        validate_knob_value(knob, value)?;
        set_json_pointer_value(&mut experiment, &knob.json_pointer, value.clone())?;
    }

    Ok(experiment)
}

fn load_experiment_overrides(overrides_path: &Path) -> Result<ExperimentOverrides> {
    let overrides_schema = compile_schema("experiment_overrides_v1.jsonschema")?;
    let overrides_data = fs::read_to_string(overrides_path)?;
    let overrides_json: Value = serde_json::from_str(&overrides_data)?;
    if let Err(errors) = overrides_schema.validate(&overrides_json) {
        let mut msgs = Vec::new();
        for e in errors {
            msgs.push(e.to_string());
        }
        return Err(anyhow!(
            "overrides schema validation failed ({}): {}",
            overrides_path.display(),
            msgs.join("; ")
        ));
    }
    let overrides: ExperimentOverrides = serde_json::from_value(overrides_json)?;
    if overrides.schema_version != "experiment_overrides_v1" {
        return Err(anyhow!(
            "unsupported overrides schema_version: {}",
            overrides.schema_version
        ));
    }
    Ok(overrides)
}

fn load_knob_manifest(manifest_path: &Path) -> Result<KnobManifest> {
    let manifest_schema = compile_schema("knob_manifest_v1.jsonschema")?;
    let manifest_data = fs::read_to_string(manifest_path)?;
    let manifest_json: Value = serde_json::from_str(&manifest_data)?;
    if let Err(errors) = manifest_schema.validate(&manifest_json) {
        let mut msgs = Vec::new();
        for e in errors {
            msgs.push(e.to_string());
        }
        return Err(anyhow!(
            "knob manifest schema validation failed ({}): {}",
            manifest_path.display(),
            msgs.join("; ")
        ));
    }
    let manifest: KnobManifest = serde_json::from_value(manifest_json)?;
    if manifest.schema_version != "knob_manifest_v1" {
        return Err(anyhow!(
            "unsupported knob manifest schema_version: {}",
            manifest.schema_version
        ));
    }
    Ok(manifest)
}

fn validate_knob_value(knob: &KnobDef, value: &Value) -> Result<()> {
    if !value_matches_type(value, &knob.value_type) {
        return Err(anyhow!(
            "override value type mismatch for knob {}: expected {}, got {}",
            knob.id,
            knob.value_type,
            value_type_name(value)
        ));
    }

    if let Some(options) = knob.options.as_ref() {
        if !options.iter().any(|opt| opt == value) {
            return Err(anyhow!(
                "override value for knob {} is not in allowed options",
                knob.id
            ));
        }
    }

    if let Some(min) = knob.minimum {
        if let Some(v) = value.as_f64() {
            if v < min {
                return Err(anyhow!(
                    "override value for knob {} is below minimum {}",
                    knob.id,
                    min
                ));
            }
        }
    }
    if let Some(max) = knob.maximum {
        if let Some(v) = value.as_f64() {
            if v > max {
                return Err(anyhow!(
                    "override value for knob {} is above maximum {}",
                    knob.id,
                    max
                ));
            }
        }
    }
    Ok(())
}

fn value_matches_type(value: &Value, t: &str) -> bool {
    match t {
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn value_type_name(value: &Value) -> &'static str {
    if value.is_string() {
        "string"
    } else if value.is_boolean() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value.is_array() {
        "array"
    } else if value.is_object() {
        "object"
    } else {
        "null"
    }
}

fn decode_pointer_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

fn set_json_pointer_value(root: &mut Value, pointer: &str, new_value: Value) -> Result<()> {
    if pointer.is_empty() || pointer == "/" {
        *root = new_value;
        return Ok(());
    }
    if !pointer.starts_with('/') {
        return Err(anyhow!("json_pointer must start with '/': {}", pointer));
    }

    let tokens: Vec<String> = pointer
        .split('/')
        .skip(1)
        .map(decode_pointer_token)
        .collect();
    if tokens.is_empty() {
        *root = new_value;
        return Ok(());
    }

    let mut cur = root;
    for token in tokens.iter().take(tokens.len() - 1) {
        match cur {
            Value::Object(map) => {
                let entry = map.entry(token.clone()).or_insert_with(|| json!({}));
                cur = entry;
            }
            Value::Array(arr) => {
                let idx: usize = token.parse().map_err(|_| {
                    anyhow!(
                        "json_pointer token '{}' is not a valid array index in {}",
                        token,
                        pointer
                    )
                })?;
                if idx >= arr.len() {
                    return Err(anyhow!(
                        "json_pointer array index {} out of bounds in {}",
                        idx,
                        pointer
                    ));
                }
                cur = &mut arr[idx];
            }
            _ => {
                return Err(anyhow!(
                    "json_pointer traversal hit non-container at token '{}' in {}",
                    token,
                    pointer
                ));
            }
        }
    }

    let last = tokens.last().unwrap();
    match cur {
        Value::Object(map) => {
            map.insert(last.clone(), new_value);
            Ok(())
        }
        Value::Array(arr) => {
            let idx: usize = last.parse().map_err(|_| {
                anyhow!(
                    "json_pointer token '{}' is not a valid array index in {}",
                    last,
                    pointer
                )
            })?;
            if idx >= arr.len() {
                return Err(anyhow!(
                    "json_pointer array index {} out of bounds in {}",
                    idx,
                    pointer
                ));
            }
            arr[idx] = new_value;
            Ok(())
        }
        _ => Err(anyhow!(
            "json_pointer target is not an object/array for {}",
            pointer
        )),
    }
}

fn resolve_dataset_path(json_value: &Value, exp_dir: &Path) -> Result<PathBuf> {
    let rel = json_value
        .pointer("/dataset/path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("dataset.path missing"))?;
    let path = exp_dir.join(rel);
    Ok(path)
}

fn load_tasks(path: &Path, json_value: &Value) -> Result<Vec<Value>> {
    let data = fs::read_to_string(path)?;
    let mut tasks = Vec::new();
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let task: Value = serde_json::from_str(line)?;
        tasks.push(task);
    }
    if let Some(limit) = json_value
        .pointer("/dataset/limit")
        .and_then(|v| v.as_u64())
    {
        tasks.truncate(limit as usize);
    }
    Ok(tasks)
}

fn count_tasks(path: &Path, json_value: &Value) -> Result<usize> {
    let data = fs::read_to_string(path)?;
    let mut count = 0usize;
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if let Some(limit) = json_value
            .pointer("/dataset/limit")
            .and_then(|v| v.as_u64())
        {
            if count >= limit as usize {
                break;
            }
        }
    }
    Ok(count)
}
#[derive(Clone)]
struct HarnessConfig {
    command_raw: Vec<String>,
    integration_level: String,
    input_path: String,
    output_path: String,
    events_path: Option<String>,
    control_path: String,
    tracing_mode: Option<String>,
    force_container: bool,
}

fn resolve_harness(json_value: &Value, _exp_dir: &Path) -> Result<HarnessConfig> {
    let harness = json_value
        .pointer("/runtime/harness")
        .ok_or_else(|| anyhow!("runtime.harness missing"))?;
    let command = harness
        .pointer("/command")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("runtime.harness.command missing"))?
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();

    let integration_level = harness
        .pointer("/integration_level")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /runtime/harness/integration_level"))?
        .to_string();
    let input_path = harness
        .pointer("/input_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /runtime/harness/input_path"))?
        .to_string();
    let output_path = harness
        .pointer("/output_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /runtime/harness/output_path"))?
        .to_string();
    let events_path = harness
        .pointer("/events/path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let control_path = harness
        .pointer("/control_plane/path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing /runtime/harness/control_plane/path"))?
        .to_string();
    let tracing_mode = harness
        .pointer("/tracing/mode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let force_container = json_value
        .pointer("/runtime/sandbox/mode")
        .and_then(|v| v.as_str())
        == Some("container");

    Ok(HarnessConfig {
        command_raw: command,
        integration_level,
        input_path,
        output_path,
        events_path,
        control_path,
        tracing_mode,
        force_container,
    })
}

struct TrialPaths {
    trial_dir: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    dataset: PathBuf,
    out: PathBuf,
    tmp: PathBuf,
    dataset_src: PathBuf,
    exp_dir: PathBuf,
}

impl TrialPaths {
    fn new(trial_dir: &Path, exp_dir: &Path, dataset_src: &Path) -> Result<Self> {
        Ok(Self {
            trial_dir: trial_dir.to_path_buf(),
            workspace: trial_dir.join("workspace"),
            state: trial_dir.join("state"),
            dataset: trial_dir.join("dataset"),
            out: trial_dir.join("out"),
            tmp: trial_dir.join("tmp"),
            dataset_src: dataset_src.to_path_buf(),
            exp_dir: exp_dir.to_path_buf(),
        })
    }

    fn prepare(&self) -> Result<()> {
        ensure_dir(&self.workspace)?;
        ensure_dir(&self.state)?;
        ensure_dir(&self.dataset)?;
        ensure_dir(&self.out)?;
        ensure_dir(&self.tmp)?;
        copy_dir_filtered(
            &self.exp_dir,
            &self.workspace,
            &[
                ".lab",
                ".git",
                "node_modules",
                ".venv",
                "__pycache__",
                ".tox",
                ".mypy_cache",
                ".pytest_cache",
                ".ruff_cache",
                "target",
                "rust/target",
                ".next",
                ".nuxt",
                ".turbo",
                ".nx",
                "coverage",
                ".gradle",
            ],
        )?;
        fs::copy(
            &self.dataset_src,
            self.dataset.join(self.dataset_src.file_name().unwrap()),
        )?;
        Ok(())
    }
}

fn build_trial_input(
    json_value: &Value,
    workload_type: &str,
    trial_id: &str,
    variant: &Variant,
    task_idx: usize,
    repl: usize,
    task: &Value,
    paths: &TrialPaths,
    container_mode: bool,
) -> Value {
    let runtime_paths = if container_mode {
        json!({
            "workspace": "/workspace",
            "state": "/state",
            "dataset": "/dataset",
            "out": "/out",
            "tmp": "/tmp",
        })
    } else {
        json!({
            "workspace": paths.workspace.to_string_lossy(),
            "state": paths.state.to_string_lossy(),
            "dataset": paths.dataset.to_string_lossy(),
            "out": paths.out.to_string_lossy(),
            "tmp": paths.tmp.to_string_lossy(),
        })
    };
    let control_path = if container_mode {
        json_value
            .pointer("/runtime/harness/control_plane/path")
            .and_then(|v| v.as_str())
            .unwrap_or("/state/lab_control.json")
            .to_string()
    } else {
        paths
            .state
            .join("lab_control.json")
            .to_string_lossy()
            .to_string()
    };
    json!({
        "schema_version": "trial_input_v1",
        "ids": {
            "run_id": json_value.pointer("/experiment/id").and_then(|v| v.as_str()).unwrap_or("run"),
            "trial_id": trial_id,
            "variant_id": variant.id,
            "task_id": task.get("id").and_then(|v| v.as_str()).unwrap_or(&format!("task_{}", task_idx)),
            "repl_idx": repl
        },
        "task": task,
        "workload": {
            "type": workload_type
        },
        "bindings": variant.bindings.clone(),
        "design": {
            "sanitization_profile": json_value.pointer("/design/sanitization_profile").and_then(|v| v.as_str()).unwrap_or("hermetic_functional_v2"),
            "integration_level": json_value.pointer("/runtime/harness/integration_level").and_then(|v| v.as_str()).unwrap_or("cli_basic"),
        },
        "runtime": {
            "paths": runtime_paths,
            "network": {
                "mode_requested": json_value.pointer("/runtime/network/mode").and_then(|v| v.as_str()).unwrap_or("none"),
                "allowed_hosts": json_value.pointer("/runtime/network/allowed_hosts").cloned().unwrap_or(json!([])),
            },
            "control_plane": {
                "mode": json_value.pointer("/runtime/harness/control_plane/mode").and_then(|v| v.as_str()).unwrap_or("file"),
                "path": control_path,
            }
        }
    })
}

fn run_harness_local(
    harness: &HarnessConfig,
    paths: &TrialPaths,
    input_path: &Path,
    output_path: &Path,
    control_path: &str,
    command: &[String],
) -> Result<String> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    cmd.current_dir(&paths.workspace);
    cmd.env("AGENTLAB_TRIAL_INPUT", &input_path);
    cmd.env("AGENTLAB_TRIAL_OUTPUT", &output_path);
    cmd.env("AGENTLAB_CONTROL_PATH", control_path);
    if harness.tracing_mode.as_deref() == Some("otlp") {
        cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318");
    }
    run_process_with_trial_io(cmd, input_path, output_path)
}

fn run_harness_container(
    json_value: &Value,
    harness: &HarnessConfig,
    paths: &TrialPaths,
    input_path: &Path,
    output_path: &Path,
    control_path: &str,
    command: &[String],
    network_mode: &str,
    setup_command: Option<&str>,
) -> Result<String> {
    let image = json_value
        .pointer("/runtime/sandbox/image")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("runtime.sandbox.image required for container mode"))?;

    if network_mode == "allowlist_enforced" {
        return Err(anyhow!("allowlist_enforced not implemented in Rust runner"));
    }

    let mut cmd = Command::new("docker");
    // Keep stdin attached so run_process_with_trial_io can pipe trial_input.json
    // into the containerized harness process.
    cmd.arg("run").arg("-i").arg("--rm");

    if json_value
        .pointer("/runtime/sandbox/root_read_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
    {
        cmd.arg("--read-only");
    }

    let run_as_user = json_value
        .pointer("/runtime/sandbox/run_as_user")
        .and_then(|v| v.as_str());
    if let Some(user) = run_as_user {
        cmd.args(["-u", user]);
    }

    if network_mode == "none" {
        cmd.arg("--network=none");
    }

    if json_value
        .pointer("/runtime/sandbox/hardening/no_new_privileges")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
    {
        cmd.args(["--security-opt", "no-new-privileges"]);
    }
    if json_value
        .pointer("/runtime/sandbox/hardening/drop_all_caps")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
    {
        cmd.args(["--cap-drop", "ALL"]);
    }

    if let Some(cpu) = json_value
        .pointer("/runtime/sandbox/resources/cpu_count")
        .and_then(|v| v.as_u64())
    {
        cmd.arg("--cpus").arg(cpu.to_string());
    }
    if let Some(mem) = json_value
        .pointer("/runtime/sandbox/resources/memory_mb")
        .and_then(|v| v.as_u64())
    {
        cmd.arg("--memory").arg(format!("{}m", mem));
    }

    cmd.args(["-v", &format!("{}:/workspace", paths.workspace.display())]);
    cmd.args(["-v", &format!("{}:/state", paths.state.display())]);
    cmd.args(["-v", &format!("{}:/dataset:ro", paths.dataset.display())]);
    cmd.args(["-v", &format!("{}:/out", paths.out.display())]);
    cmd.args(["--tmpfs", "/tmp:rw"]);
    cmd.args(["-w", "/workspace"]);

    cmd.arg("-e")
        .arg(format!("AGENTLAB_TRIAL_INPUT={}", harness.input_path));
    cmd.arg("-e")
        .arg(format!("AGENTLAB_TRIAL_OUTPUT={}", harness.output_path));
    cmd.arg("-e")
        .arg(format!("AGENTLAB_CONTROL_PATH={}", control_path));

    if harness.tracing_mode.as_deref() == Some("otlp") {
        cmd.arg("-e")
            .arg("OTEL_EXPORTER_OTLP_ENDPOINT=http://host.docker.internal:4318");
        #[cfg(target_os = "linux")]
        {
            cmd.arg("--add-host")
                .arg("host.docker.internal:host-gateway");
        }
    }

    cmd.arg(image);
    if let Some(setup) = setup_command {
        let mut script_parts = Vec::new();
        script_parts.push(setup.to_string());
        script_parts.push(shell_join(command));
        let script = script_parts.join(" && ");
        cmd.arg("sh").arg("-lc").arg(script);
    } else {
        cmd.args(command);
    }
    run_process_with_trial_io(cmd, input_path, output_path)
}

fn resolve_command_local(command: &[String], exp_dir: &Path) -> Vec<String> {
    let mut resolved = Vec::new();
    for part in command {
        let p = Path::new(part);
        if p.is_relative() && command_part_looks_like_path(part) {
            resolved.push(
                normalize_path(&exp_dir.join(p))
                    .to_string_lossy()
                    .to_string(),
            );
        } else {
            resolved.push(part.clone());
        }
    }
    resolved
}

fn resolve_command_container(command: &[String], exp_dir: &Path) -> Vec<String> {
    let mut resolved = Vec::new();
    for part in command {
        let p = Path::new(part);
        if p.is_relative() && command_part_looks_like_path(part) {
            let rel = p.to_string_lossy().trim_start_matches("./").to_string();
            resolved.push(format!("/workspace/{}", rel));
        } else if p.is_absolute() && p.starts_with(exp_dir) {
            if let Ok(rel) = p.strip_prefix(exp_dir) {
                let rel = rel.to_string_lossy().trim_start_matches('/').to_string();
                resolved.push(format!("/workspace/{}", rel));
            } else {
                resolved.push(part.clone());
            }
        } else {
            resolved.push(part.clone());
        }
    }
    resolved
}

fn resolve_command_script_path(command: &[String], project_root: &Path) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    let candidate_idx = if command_part_looks_like_path(&command[0]) {
        0
    } else if command.len() >= 2 && command_part_looks_like_path(&command[1]) {
        1
    } else {
        return None;
    };
    let candidate = Path::new(&command[candidate_idx]);
    if candidate.is_absolute() {
        return Some(normalize_path(candidate));
    }
    if candidate.as_os_str().is_empty() {
        return None;
    }
    Some(normalize_path(&project_root.join(candidate)))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn validate_harness_command(command: &[String], project_root: &Path) -> Result<()> {
    if command.is_empty() {
        return Ok(());
    }
    let path = resolve_command_script_path(command, project_root);
    if let Some(p) = path {
        if !p.exists() {
            let mut candidates: Vec<String> = Vec::new();
            for c in [
                "harness.js",
                "agentlab_demo_harness.js",
                "agentlab/harness.js",
                "harness.py",
                "main.py",
            ] {
                let cp = project_root.join(c);
                if cp.exists() {
                    candidates.push(cp.display().to_string());
                }
            }
            let hint = if candidates.is_empty() {
                "no common harness entrypoints found".to_string()
            } else {
                format!("candidates: {}", candidates.join(", "))
            };
            return Err(anyhow!(
                "harness command file not found: {} (update runtime.harness.command). {}",
                p.display(),
                hint
            ));
        }
    }
    Ok(())
}

fn run_process_with_trial_io(
    mut cmd: Command,
    input_path: &Path,
    output_path: &Path,
) -> Result<String> {
    let input_bytes = fs::read(input_path).unwrap_or_default();
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&input_bytes);
    }
    let output = child.wait_with_output()?;

    if !output_path.exists() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let maybe_json = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.trim().to_string());
        if let Some(line) = maybe_json {
            if serde_json::from_str::<Value>(&line).is_ok() {
                if let Some(parent) = output_path.parent() {
                    ensure_dir(parent)?;
                }
                atomic_write_bytes(output_path, line.as_bytes())?;
            }
        }
    }

    if !output_path.exists() {
        let ids = serde_json::from_slice::<Value>(&input_bytes)
            .ok()
            .and_then(|v| v.get("ids").cloned())
            .unwrap_or(json!({}));
        let stderr_tail = String::from_utf8_lossy(&output.stderr)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("harness exited without writing trial_output")
            .to_string();
        let fallback = json!({
            "schema_version": "trial_output_v1",
            "ids": ids,
            "outcome": "error",
            "error": {
                "error_type": "harness_process_error",
                "message": stderr_tail
            }
        });
        if let Some(parent) = output_path.parent() {
            ensure_dir(parent)?;
        }
        let fallback_bytes = serde_json::to_vec_pretty(&fallback)?;
        atomic_write_bytes(output_path, &fallback_bytes)?;
    }

    Ok(output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string()))
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        "''".to_string()
    } else if s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

fn prepare_io_paths(
    paths: &TrialPaths,
    container_mode: bool,
    input_bytes: &[u8],
) -> Result<(PathBuf, PathBuf)> {
    let input_host = if container_mode {
        let path = paths.out.join("trial_input.json");
        fs::write(&path, input_bytes)?;
        path
    } else {
        paths.trial_dir.join("trial_input.json")
    };
    let output_host = if container_mode {
        paths.out.join("trial_output.json")
    } else {
        paths.trial_dir.join("trial_output.json")
    };
    Ok((input_host, output_host))
}

fn resolve_control_paths(
    control_path: &str,
    paths: &TrialPaths,
    container_mode: bool,
) -> (String, PathBuf) {
    if container_mode {
        let host_path = map_container_path_to_host(control_path, paths);
        (control_path.to_string(), host_path)
    } else {
        let host = paths.state.join("lab_control.json");
        (host.to_string_lossy().to_string(), host)
    }
}

fn write_control_file(path: &Path) -> Result<()> {
    let _ = write_control_action(path, 0, "continue", None, "run_loop")?;
    Ok(())
}

fn write_control_action(
    path: &Path,
    seq: u64,
    action: &str,
    label: Option<&str>,
    requested_by: &str,
) -> Result<String> {
    let payload = json!({
        "schema_version": "control_plane_v1",
        "seq": seq,
        "action": action,
        "label": label,
        "requested_at": Utc::now().to_rfc3339(),
        "requested_by": requested_by,
    });
    let bytes = serde_json::to_vec_pretty(&payload)?;
    let version = sha256_bytes(&bytes);
    atomic_write_bytes(path, &bytes)?;
    Ok(version)
}

fn resolve_event_path(events_path: &str, paths: &TrialPaths, _container_mode: bool) -> PathBuf {
    if events_path.starts_with("/out")
        || events_path.starts_with("/state")
        || events_path.starts_with("/workspace")
        || events_path.starts_with("/dataset")
        || events_path.starts_with("/tmp")
    {
        map_container_path_to_host(events_path, paths)
    } else {
        let p = Path::new(events_path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            paths.workspace.join(p)
        }
    }
}

fn resolve_harness_manifest_path(paths: &TrialPaths, container_mode: bool) -> PathBuf {
    if container_mode {
        map_container_path_to_host("/out/harness_manifest.json", paths)
    } else {
        let direct = paths.trial_dir.join("harness_manifest.json");
        if direct.exists() {
            direct
        } else if paths.workspace.join("harness_manifest.json").exists() {
            paths.workspace.join("harness_manifest.json")
        } else {
            paths.out.join("harness_manifest.json")
        }
    }
}

fn resolve_exec_digest(command: &[String], exp_dir: &Path) -> Result<String> {
    if let Some(candidate_part) = resolve_command_digest_target(command) {
        let candidate = Path::new(candidate_part);
        let host_path = if candidate.is_relative() {
            exp_dir.join(candidate)
        } else {
            candidate.to_path_buf()
        };
        if host_path.exists() && host_path.is_file() {
            return sha256_file(&host_path);
        }
    }
    Ok(sha256_bytes(command.join(" ").as_bytes()))
}

fn write_state_inventory(
    trial_dir: &Path,
    json_value: &Value,
    harness: &HarnessConfig,
    container_mode: bool,
    paths: &TrialPaths,
    exec_digest: &str,
    effective_network_mode: &str,
) -> Result<()> {
    let sanitization_profile = json_value
        .pointer("/design/sanitization_profile")
        .and_then(|v| v.as_str())
        .unwrap_or("hermetic_functional_v2");
    let integration_level = harness.integration_level.as_str();
    let mode_requested = json_value
        .pointer("/runtime/network/mode")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    let mode_effective = if container_mode {
        effective_network_mode
    } else {
        "full"
    };
    let enforcement_effective = if container_mode && mode_requested == "none" {
        "docker_none"
    } else {
        "unknown"
    };

    let mounts = if container_mode {
        vec![
            json!({"name": "workspace", "path": "/workspace", "writable": true}),
            json!({"name": "state", "path": "/state", "writable": true}),
            json!({"name": "dataset", "path": "/dataset", "writable": false}),
            json!({"name": "out", "path": "/out", "writable": true}),
            json!({"name": "tmp", "path": "/tmp", "writable": true}),
        ]
    } else {
        vec![
            json!({"name": "workspace", "path": paths.workspace.to_string_lossy(), "writable": true}),
            json!({"name": "state", "path": paths.state.to_string_lossy(), "writable": true}),
            json!({"name": "dataset", "path": paths.dataset.to_string_lossy(), "writable": false}),
            json!({"name": "out", "path": paths.out.to_string_lossy(), "writable": true}),
            json!({"name": "tmp", "path": paths.tmp.to_string_lossy(), "writable": true}),
        ]
    };

    let state = json!({
        "schema_version": "state_inventory_v1",
        "sanitization_profile": sanitization_profile,
        "integration_level": integration_level,
        "mounts": mounts,
        "network": {
            "mode_requested": mode_requested,
            "mode_effective": mode_effective,
            "allowed_hosts": json_value.pointer("/runtime/network/allowed_hosts").cloned().unwrap_or(json!([])),
            "enforcement_effective": enforcement_effective,
            "egress_self_test": {
                "performed": false,
                "cases": []
            }
        },
        "harness_identity": {
            "name": harness.command_raw.get(0).cloned().unwrap_or("unknown".to_string()),
            "exec_digest": exec_digest,
            "entry_command": harness.command_raw.clone()
        },
        "violations": {
            "state_leak": false,
            "profile_invariant_violation": false,
            "notes": []
        }
    });
    atomic_write_json_pretty(&trial_dir.join("state_inventory.json"), &state)?;
    Ok(())
}

fn map_container_path_to_host(path: &str, paths: &TrialPaths) -> PathBuf {
    if let Some(rest) = path.strip_prefix("/state") {
        paths.state.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = path.strip_prefix("/out") {
        paths.out.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = path.strip_prefix("/workspace") {
        paths.workspace.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = path.strip_prefix("/dataset") {
        paths.dataset.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = path.strip_prefix("/tmp") {
        paths.tmp.join(rest.trim_start_matches('/'))
    } else {
        paths.trial_dir.join(path.trim_start_matches('/'))
    }
}

fn count_event_types(events_path: &Path) -> Result<BTreeMap<String, usize>> {
    let data = fs::read_to_string(events_path)?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)?;
        if let Some(et) = v.get("event_type").and_then(|v| v.as_str()) {
            *counts.entry(et.to_string()).or_default() += 1;
        }
    }
    Ok(counts)
}

fn copy_dir_filtered(src: &Path, dst: &Path, exclude: &[&str]) -> Result<()> {
    let walker = walkdir::WalkDir::new(src).into_iter().filter_entry(|e| {
        let rel = e.path().strip_prefix(src).unwrap_or(e.path());
        if rel.as_os_str().is_empty() {
            return true; // root entry
        }
        !exclude.iter().any(|ex| rel.starts_with(ex))
    });
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(src).unwrap();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            ensure_dir(&target)?;
        } else if entry.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            match fs::canonicalize(path) {
                Ok(real) if real.is_dir() => {
                    copy_dir_filtered(&real, &target, &[])?;
                }
                Ok(real) if real.is_file() => {
                    fs::copy(real, &target)?;
                }
                Ok(_) => {}
                Err(_) => {
                    // Preserve broken links instead of aborting trial setup.
                    let link_target = fs::read_link(path)?;
                    if target.exists() {
                        let _ = fs::remove_file(&target);
                    }
                    #[cfg(unix)]
                    {
                        symlink(&link_target, &target)?;
                    }
                }
            }
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            fs::copy(path, target)?;
        }
    }
    Ok(())
}

fn command_part_looks_like_path(part: &str) -> bool {
    part.starts_with('.')
        || part.starts_with('/')
        || part.contains('/')
        || part.ends_with(".js")
        || part.ends_with(".mjs")
        || part.ends_with(".cjs")
        || part.ends_with(".ts")
        || part.ends_with(".py")
        || part.ends_with(".sh")
}

fn resolve_command_digest_target(command: &[String]) -> Option<&str> {
    if command.is_empty() {
        return None;
    }
    if command_part_looks_like_path(&command[0]) {
        return Some(command[0].as_str());
    }
    if command.len() >= 2 && command_part_looks_like_path(&command[1]) {
        return Some(command[1].as_str());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_script_path_supports_binary_first_commands() {
        let root = PathBuf::from("/tmp/agentlab_proj");
        let cmd = vec!["./harness".to_string(), "run".to_string()];
        let resolved = resolve_command_script_path(&cmd, &root).expect("expected path");
        assert_eq!(resolved, normalize_path(&root.join("harness")));
    }

    #[test]
    fn resolve_script_path_supports_interpreter_plus_script() {
        let root = PathBuf::from("/tmp/agentlab_proj");
        let cmd = vec![
            "node".to_string(),
            "./harness.js".to_string(),
            "run".to_string(),
        ];
        let resolved = resolve_command_script_path(&cmd, &root).expect("expected path");
        assert_eq!(resolved, normalize_path(&root.join("harness.js")));
    }

    #[test]
    fn resolve_command_local_resolves_first_token_when_path_like() {
        let root = PathBuf::from("/tmp/agentlab_proj");
        let cmd = vec!["./harness".to_string(), "run".to_string()];
        let resolved = resolve_command_local(&cmd, &root);
        assert_eq!(resolved[0], root.join("harness").to_string_lossy());
        assert_eq!(resolved[1], "run");
    }

    #[test]
    fn replay_grade_maps_by_integration_level() {
        assert_eq!(replay_grade_for_integration("sdk_full"), "strict");
        assert_eq!(replay_grade_for_integration("sdk_control"), "checkpointed");
        assert_eq!(replay_grade_for_integration("cli_events"), "best_effort");
        assert_eq!(replay_grade_for_integration("cli_basic"), "best_effort");
    }

    #[test]
    fn run_operation_lock_is_exclusive() {
        let run_dir = std::env::temp_dir().join(format!(
            "agentlab_lock_test_{}_{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        ensure_dir(&run_dir).expect("temp run dir");

        let lock1 = acquire_run_operation_lock(&run_dir).expect("first lock must succeed");
        let err = acquire_run_operation_lock(&run_dir).expect_err("second lock must fail");
        assert!(
            err.to_string().contains("operation_in_progress"),
            "unexpected lock error: {}",
            err
        );
        drop(lock1);
        let lock2 = acquire_run_operation_lock(&run_dir).expect("lock should be re-acquirable");
        drop(lock2);
        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn fork_selector_parser_accepts_supported_kinds() {
        match parse_fork_selector("checkpoint:ckpt_a").expect("checkpoint selector") {
            ForkSelector::Checkpoint(v) => assert_eq!(v, "ckpt_a"),
            _ => panic!("expected checkpoint"),
        }
        match parse_fork_selector("step:12").expect("step selector") {
            ForkSelector::Step(v) => assert_eq!(v, 12),
            _ => panic!("expected step"),
        }
        match parse_fork_selector("event_seq:34").expect("event_seq selector") {
            ForkSelector::EventSeq(v) => assert_eq!(v, 34),
            _ => panic!("expected event_seq"),
        }
        assert!(parse_fork_selector("bad").is_err());
        assert!(parse_fork_selector("unknown:1").is_err());
    }

    #[test]
    fn has_control_ack_matches_action_and_control_version() {
        let root = std::env::temp_dir().join(format!(
            "agentlab_ack_test_{}_{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        ensure_dir(&root).expect("temp dir");
        let events_path = root.join("harness_events.jsonl");
        let line = r#"{"event_type":"control_ack","seq":9,"step_index":2,"control_version":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","action_observed":"stop"}"#;
        atomic_write_bytes(&events_path, format!("{}\n", line).as_bytes()).expect("write events");

        assert!(has_control_ack(
            &events_path,
            "stop",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .expect("parse ack"));
        assert!(!has_control_ack(
            &events_path,
            "checkpoint",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )
        .expect("parse ack"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_resume_selector_prefers_requested_label() {
        let root = std::env::temp_dir().join(format!(
            "agentlab_resume_sel_test_{}_{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        ensure_dir(&root).expect("root");
        let trial_dir = root.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let output = json!({
            "schema_version": "trial_output_v1",
            "outcome": "success",
            "checkpoints": [
                {"path": "/state/ckpt_a", "logical_name": "a", "step": 1},
                {"path": "/state/ckpt_b", "logical_name": "b", "step": 2}
            ]
        });
        atomic_write_json_pretty(&trial_dir.join("trial_output.json"), &output).expect("write");
        let selector = resolve_resume_selector(&trial_dir, Some("a")).expect("selector");
        assert_eq!(selector, "checkpoint:a");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_resume_selector_defaults_to_latest_step() {
        let root = std::env::temp_dir().join(format!(
            "agentlab_resume_default_test_{}_{}",
            std::process::id(),
            Utc::now().timestamp_micros()
        ));
        ensure_dir(&root).expect("root");
        let trial_dir = root.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let output = json!({
            "schema_version": "trial_output_v1",
            "outcome": "success",
            "checkpoints": [
                {"path": "/state/ckpt_a", "logical_name": "a", "step": 3},
                {"path": "/state/ckpt_b", "logical_name": "b", "step": 5}
            ]
        });
        atomic_write_json_pretty(&trial_dir.join("trial_output.json"), &output).expect("write");
        let selector = resolve_resume_selector(&trial_dir, None).expect("selector");
        assert_eq!(selector, "checkpoint:b");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validate_required_fields_passes_on_complete_spec() {
        let spec = json!({
            "version": "0.3",
            "experiment": { "id": "e", "name": "n", "workload_type": "agent_harness" },
            "dataset": { "path": "tasks.jsonl", "provider": "local_jsonl", "suite_id": "s", "schema_version": "v1", "split_id": "dev", "limit": 50 },
            "design": { "sanitization_profile": "hermetic_functional_v2", "comparison": "paired", "replications": 1, "random_seed": 1337, "shuffle_tasks": true, "max_concurrency": 1 },
            "baseline": { "variant_id": "base", "bindings": {} },
            "runtime": {
                "harness": { "mode": "cli", "command": ["node", "h.js"], "integration_level": "cli_basic", "input_path": "/out/in.json", "output_path": "/out/out.json", "control_plane": { "mode": "file", "path": "/state/ctl.json" } },
                "sandbox": { "mode": "local" },
                "network": { "mode": "none", "allowed_hosts": [] }
            }
        });
        validate_required_fields(&spec).expect("valid spec should pass");
    }

    #[test]
    fn validate_required_fields_reports_all_missing() {
        let spec = json!({
            "version": "0.3",
            "experiment": { "id": "e", "name": "n" },
            "dataset": { "path": "tasks.jsonl" },
            "design": {},
            "baseline": {},
            "runtime": { "harness": { "mode": "cli" }, "sandbox": { "mode": "local" }, "network": {} }
        });
        let err = validate_required_fields(&spec).expect_err("should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("/experiment/workload_type"),
            "missing workload_type: {}",
            msg
        );
        assert!(
            msg.contains("/design/sanitization_profile"),
            "missing sanitization_profile: {}",
            msg
        );
        assert!(
            msg.contains("/design/replications"),
            "missing replications: {}",
            msg
        );
        assert!(
            msg.contains("/runtime/harness/command"),
            "missing command: {}",
            msg
        );
        assert!(
            msg.contains("/runtime/harness/integration_level"),
            "missing integration_level: {}",
            msg
        );
        assert!(
            msg.contains("/runtime/network/mode"),
            "missing network mode: {}",
            msg
        );
        assert!(
            msg.contains("/baseline/variant_id"),
            "missing baseline variant_id: {}",
            msg
        );
    }

    #[test]
    fn validate_required_fields_reports_subset() {
        let spec = json!({
            "version": "0.3",
            "experiment": { "id": "e", "name": "n", "workload_type": "agent_harness" },
            "dataset": { "path": "tasks.jsonl", "provider": "local_jsonl", "suite_id": "s", "schema_version": "v1", "split_id": "dev", "limit": 50 },
            "design": { "sanitization_profile": "hermetic_functional_v2", "comparison": "paired", "replications": 1, "random_seed": 1337, "shuffle_tasks": true, "max_concurrency": 1 },
            "baseline": { "variant_id": "base", "bindings": {} },
            "runtime": {
                "harness": { "mode": "cli", "command": ["node", "h.js"], "input_path": "/out/in.json", "output_path": "/out/out.json", "control_plane": { "mode": "file", "path": "/state/ctl.json" } },
                "sandbox": { "mode": "local" },
                "network": { "mode": "none", "allowed_hosts": [] }
            }
        });
        let err = validate_required_fields(&spec).expect_err("should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("/runtime/harness/integration_level"),
            "should report integration_level: {}",
            msg
        );
        assert!(
            !msg.contains("/experiment/workload_type"),
            "should not report workload_type: {}",
            msg
        );
    }
}
