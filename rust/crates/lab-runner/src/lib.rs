use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::Utc;
use lab_analysis::{summarize_trial, write_analysis};
use lab_core::{canonical_json_digest, ensure_dir, sha256_bytes, sha256_file, ArtifactStore};
use lab_hooks::{load_manifest, validate_hooks};
use lab_provenance::{default_attestation, write_attestation};
use lab_schemas::compile_schema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorKind {
    LocalDocker,
    LocalProcess,
    Remote,
}

impl ExecutorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalDocker => "local_docker",
            Self::LocalProcess => "local_process",
            Self::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationMode {
    None,
    MetadataOnly,
    OutputsOnly,
    Full,
}

impl MaterializationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MetadataOnly => "metadata_only",
            Self::OutputsOnly => "outputs_only",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunExecutionOptions {
    pub executor: Option<ExecutorKind>,
    pub materialize: Option<MaterializationMode>,
    pub remote_endpoint: Option<String>,
    pub remote_token_env: Option<String>,
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
    pub scheduling: String,
    pub state_policy: String,
    pub comparison: String,
    pub retry_max_attempts: usize,
}

pub fn run_experiment(path: &Path, use_container: bool) -> Result<RunResult> {
    run_experiment_with_behavior(
        path,
        use_container,
        RunBehavior::default(),
        None,
        RunExecutionOptions::default(),
    )
}

pub fn run_experiment_dev(path: &Path, setup_command: Option<String>) -> Result<RunResult> {
    run_experiment_dev_with_overrides(path, setup_command, None)
}

pub fn run_experiment_with_overrides(
    path: &Path,
    use_container: bool,
    overrides_path: Option<&Path>,
) -> Result<RunResult> {
    run_experiment_with_behavior(
        path,
        use_container,
        RunBehavior::default(),
        overrides_path,
        RunExecutionOptions::default(),
    )
}

pub fn run_experiment_with_options_and_overrides(
    path: &Path,
    use_container: bool,
    overrides_path: Option<&Path>,
    options: RunExecutionOptions,
) -> Result<RunResult> {
    run_experiment_with_behavior(
        path,
        use_container,
        RunBehavior::default(),
        overrides_path,
        options,
    )
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
    run_experiment_with_behavior(
        path,
        true,
        behavior,
        overrides_path,
        RunExecutionOptions::default(),
    )
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
    run_experiment_with_behavior(
        path,
        true,
        behavior,
        overrides_path,
        RunExecutionOptions::default(),
    )
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
    let task_boundary = parse_task_boundary_from_trial_input(&input)?;

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
    materialize_workspace_files(&trial_paths, &task_boundary.workspace_files)?;

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
    let dynamic_mounts = resolve_task_mounts(
        &project_root,
        &task_boundary.mount_references,
        container_mode,
    )?;

    let effective_network_mode = input
        .pointer("/runtime/network/mode_requested")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let proc_result = if container_mode {
        let command = resolve_command_container(&harness.command_raw, &project_root);
        run_harness_container(
            &json_value,
            &harness,
            &trial_paths,
            &dynamic_mounts,
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
    let status = proc_result.status;

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
    let task_boundary = parse_task_boundary_from_trial_input(&input)?;

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
    materialize_workspace_files(&trial_paths, &task_boundary.workspace_files)?;

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
    let dynamic_mounts = resolve_task_mounts(
        &project_root,
        &task_boundary.mount_references,
        container_mode,
    )?;

    let effective_network_mode = input
        .pointer("/runtime/network/mode_requested")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let proc_result = if container_mode {
        let command = resolve_command_container(&harness.command_raw, &project_root);
        run_harness_container(
            &json_value,
            &harness,
            &trial_paths,
            &dynamic_mounts,
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
    let status = proc_result.status;

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
    execution: RunExecutionOptions,
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

    let materialize_mode = execution.materialize.unwrap_or(MaterializationMode::Full);
    if matches!(execution.executor, Some(ExecutorKind::Remote)) {
        let endpoint = execution
            .remote_endpoint
            .as_deref()
            .ok_or_else(|| anyhow!("remote executor requires --remote-endpoint"))?;
        let token_env = execution.remote_token_env.as_deref().unwrap_or("unset");
        return Err(anyhow!(
            "remote executor is not implemented yet (endpoint: {}, token_env: {})",
            endpoint,
            token_env
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

    let evidence_dir = run_dir.join("evidence");
    ensure_dir(&evidence_dir)?;
    let evidence_records_path = evidence_dir.join("evidence_records.jsonl");
    let task_chain_states_path = evidence_dir.join("task_chain_states.jsonl");
    let artifact_store = ArtifactStore::new(run_dir.join("artifacts"));
    let benchmark_config = parse_benchmark_config(&json_value);

    let harness = resolve_harness(&json_value, &project_root)?;
    validate_harness_command(&harness.command_raw, &project_root)?;
    let executor_kind = execution.executor.unwrap_or_else(|| {
        if use_container || harness.force_container {
            ExecutorKind::LocalDocker
        } else {
            ExecutorKind::LocalProcess
        }
    });
    let container_mode = matches!(executor_kind, ExecutorKind::LocalDocker);

    let mut trial_summaries = Vec::new();
    let mut event_counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut trial_event_counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();

    let policy_config = parse_policies(&json_value);
    let random_seed = json_value
        .pointer("/design/random_seed")
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let schedule = build_trial_schedule(
        variants.len(),
        tasks.len(),
        replications,
        policy_config.scheduling,
        random_seed,
    );

    // Per-variant consecutive failure tracking (for pruning)
    let mut consecutive_failures: BTreeMap<usize, usize> = BTreeMap::new();
    let mut pruned_variants: HashSet<usize> = HashSet::new();
    let mut chain_states: BTreeMap<String, ChainRuntimeState> = BTreeMap::new();

    let mut trial_index: usize = 0;
    let mut run_paused = false;
    'schedule: for slot in &schedule {
        // Skip pruned variants
        if pruned_variants.contains(&slot.variant_idx) {
            continue;
        }

        let variant = &variants[slot.variant_idx];
        let task_idx = slot.task_idx;
        let task = &tasks[task_idx];
        let task_boundary = parse_task_boundary_from_dataset_task(task)?;
        let repl = slot.repl_idx;
        let task_id = task_boundary
            .task_payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("task_{}", task_idx));
        let effective_policy = resolve_effective_task_policy(
            &policy_config,
            &benchmark_config.policy,
            &task_boundary.task_payload,
        );
        let chain_label = resolve_chain_label(
            &task_boundary.task_payload,
            &task_id,
            effective_policy.state_policy,
        );
        let chain_key = format!("{}::{}", variant.id, chain_label);
        let chain_fs_key = sanitize_for_fs(&chain_key);
        let chain_step_index = chain_states
            .get(&chain_key)
            .map(|state| state.step_index + 1)
            .unwrap_or(0);

        trial_index += 1;
        let trial_id = format!("trial_{}", trial_index);
        let trial_dir = trials_dir.join(&trial_id);
        ensure_dir(&trial_dir)?;
        write_trial_state(&trial_dir, &trial_id, "running", None, None, None)?;
        let mut trial_guard = TrialStateGuard::new(&trial_dir, &trial_id);

        let trial_paths = TrialPaths::new(&trial_dir, &project_root, &dataset_path)?;

        trial_paths.prepare()?;
        if !matches!(effective_policy.state_policy, StatePolicy::IsolatePerTrial) {
            if let Some(chain_state) = chain_states.get(&chain_key) {
                restore_workspace_from_snapshot(
                    &chain_state.latest_snapshot_path,
                    &trial_paths.workspace,
                )?;
            }
        }

        materialize_workspace_files(&trial_paths, &task_boundary.workspace_files)?;
        let dynamic_mounts = resolve_task_mounts(
            &project_root,
            &task_boundary.mount_references,
            container_mode,
        )?;

        let input = build_trial_input(
            &json_value,
            &run_id,
            &workload_type,
            &trial_id,
            variant,
            task_idx,
            repl,
            &task_boundary,
            &trial_paths,
            container_mode,
        );
        let input_bytes = serde_json::to_vec_pretty(&input)?;
        let canonical_input_path = trial_dir.join("trial_input.json");
        atomic_write_bytes(&canonical_input_path, &input_bytes)?;

        let trial_metadata = json!({
            "schema_version": "trial_metadata_v1",
            "ids": {
                "run_id": run_id.as_str(),
                "trial_id": trial_id.as_str(),
                "variant_id": variant.id.as_str(),
                "task_id": task_id.as_str(),
                "repl_idx": repl
            },
            "policy_merge": {
                "global_defaults": {
                    "state_policy": "isolate_per_trial",
                    "task_model": "independent",
                    "scoring_lifecycle": "predict_then_score",
                    "required_evidence_classes": []
                },
                "experiment_type_policy": {
                    "state_policy": match policy_config.state {
                        StatePolicy::IsolatePerTrial => "isolate_per_trial",
                        StatePolicy::PersistPerTask => "persist_per_task",
                        StatePolicy::Accumulate => "accumulate",
                    }
                },
                "benchmark_type_policy": {
                    "task_model": benchmark_config.policy.task_model.as_str(),
                    "scoring_lifecycle": benchmark_config.policy.scoring_lifecycle.as_str(),
                    "required_evidence_classes": benchmark_config.policy.required_evidence_classes.clone()
                },
                "task_override": task_boundary.task_payload.get("policy_override").cloned(),
                "effective": {
                    "state_policy": match effective_policy.state_policy {
                        StatePolicy::IsolatePerTrial => "isolate_per_trial",
                        StatePolicy::PersistPerTask => "persist_per_task",
                        StatePolicy::Accumulate => "accumulate",
                    },
                    "task_model": effective_policy.task_model.as_str(),
                    "scoring_lifecycle": effective_policy.scoring_lifecycle.as_str(),
                    "required_evidence_classes": effective_policy.required_evidence_classes.clone(),
                    "chain_failure_policy": effective_policy.chain_failure_policy.as_str(),
                }
            },
            "chain": {
                "chain_id": chain_key.as_str(),
                "step_index": chain_step_index
            }
        });
        atomic_write_json_pretty(&trial_dir.join("trial_metadata.json"), &trial_metadata)?;

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

        let trial_evidence_dir = trial_dir.join("evidence");
        ensure_dir(&trial_evidence_dir)?;
        let chains_dir = evidence_dir.join("chains").join(&chain_fs_key);
        ensure_dir(&chains_dir)?;

        let pre_snapshot_manifest = collect_workspace_snapshot_manifest(&trial_paths.workspace)?;
        let pre_snapshot_path = trial_evidence_dir.join("workspace_pre_snapshot.json");
        atomic_write_json_pretty(&pre_snapshot_path, &pre_snapshot_manifest)?;
        let pre_snapshot_ref = artifact_store.put_file(&pre_snapshot_path)?;

        let (chain_root_snapshot_ref, chain_root_snapshot_path) =
            if let Some(existing) = chain_states.get(&chain_key) {
                (
                    existing.chain_root_snapshot_ref.clone(),
                    existing.chain_root_snapshot_path.clone(),
                )
            } else {
                let root_workspace = chains_dir.join("chain_root_workspace");
                if root_workspace.exists() {
                    fs::remove_dir_all(&root_workspace)?;
                }
                ensure_dir(&root_workspace)?;
                copy_dir_filtered(&trial_paths.workspace, &root_workspace, &[])?;
                (pre_snapshot_ref.clone(), root_workspace)
            };

        // Retry loop
        let mut status = String::new();
        let mut trial_output: Value =
            json!({"schema_version": "trial_output_v1", "outcome": "error"});
        let trial_started_at = Instant::now();
        for attempt in 0..policy_config.retry_max_attempts {
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

            let proc_result = if matches!(executor_kind, ExecutorKind::LocalDocker) {
                let command = resolve_command_container(&harness.command_raw, &project_root);
                run_harness_container(
                    &json_value,
                    &harness,
                    &trial_paths,
                    &dynamic_mounts,
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
            status = proc_result.status;
            atomic_write_bytes(
                &trial_dir.join("harness_stdout.log"),
                proc_result.stdout.as_bytes(),
            )?;
            atomic_write_bytes(
                &trial_dir.join("harness_stderr.log"),
                proc_result.stderr.as_bytes(),
            )?;

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
            trial_output = if canonical_output.exists() {
                serde_json::from_slice(&fs::read(&canonical_output)?)?
            } else {
                json!({"schema_version": "trial_output_v1", "outcome": "error"})
            };

            let outcome = trial_output
                .get("outcome")
                .and_then(|v| v.as_str())
                .unwrap_or("error");

            // Check if retry is needed (skip on last attempt)
            let is_last_attempt = attempt + 1 >= policy_config.retry_max_attempts;
            if !is_last_attempt && should_retry_outcome(outcome, &status, &policy_config.retry_on) {
                continue; // retry
            }
            break; // success or exhausted retries
        }

        let post_snapshot_manifest = collect_workspace_snapshot_manifest(&trial_paths.workspace)?;
        let post_snapshot_path = trial_evidence_dir.join("workspace_post_snapshot.json");
        atomic_write_json_pretty(&post_snapshot_path, &post_snapshot_manifest)?;
        let post_snapshot_ref = artifact_store.put_file(&post_snapshot_path)?;

        let chain_root_snapshot_manifest =
            collect_workspace_snapshot_manifest(&chain_root_snapshot_path)?;

        let diff_incremental = diff_workspace_snapshots(&pre_snapshot_manifest, &post_snapshot_manifest);
        let diff_cumulative = diff_workspace_snapshots(&chain_root_snapshot_manifest, &post_snapshot_manifest);
        let patch_incremental = derive_patch_from_diff(&diff_incremental);
        let patch_cumulative = derive_patch_from_diff(&diff_cumulative);

        let diff_incremental_path = trial_evidence_dir.join("workspace_diff_incremental.json");
        let diff_cumulative_path = trial_evidence_dir.join("workspace_diff_cumulative.json");
        let patch_incremental_path = trial_evidence_dir.join("workspace_patch_incremental.json");
        let patch_cumulative_path = trial_evidence_dir.join("workspace_patch_cumulative.json");
        atomic_write_json_pretty(&diff_incremental_path, &diff_incremental)?;
        atomic_write_json_pretty(&diff_cumulative_path, &diff_cumulative)?;
        atomic_write_json_pretty(&patch_incremental_path, &patch_incremental)?;
        atomic_write_json_pretty(&patch_cumulative_path, &patch_cumulative)?;

        let diff_incremental_ref = artifact_store.put_file(&diff_incremental_path)?;
        let diff_cumulative_ref = artifact_store.put_file(&diff_cumulative_path)?;
        let patch_incremental_ref = artifact_store.put_file(&patch_incremental_path)?;
        let patch_cumulative_ref = artifact_store.put_file(&patch_cumulative_path)?;

        let post_workspace_snapshot_dir = chains_dir.join(format!(
            "step_{:06}_{}_workspace",
            chain_step_index,
            sanitize_for_fs(&trial_id)
        ));
        if post_workspace_snapshot_dir.exists() {
            fs::remove_dir_all(&post_workspace_snapshot_dir)?;
        }
        ensure_dir(&post_workspace_snapshot_dir)?;
        copy_dir_filtered(&trial_paths.workspace, &post_workspace_snapshot_dir, &[])?;

        if !matches!(effective_policy.state_policy, StatePolicy::IsolatePerTrial) {
            chain_states.insert(
                chain_key.clone(),
                ChainRuntimeState {
                    chain_root_snapshot_ref: chain_root_snapshot_ref.clone(),
                    chain_root_snapshot_path: chain_root_snapshot_path.clone(),
                    latest_snapshot_ref: post_snapshot_ref.clone(),
                    latest_snapshot_path: post_workspace_snapshot_dir.clone(),
                    step_index: chain_step_index,
                },
            );
        }

        let canonical_output = trial_dir.join("trial_output.json");
        let trial_input_ref = artifact_store.put_file(&canonical_input_path)?;
        let trial_output_ref = artifact_store.put_file(&canonical_output)?;

        let stdout_path = trial_dir.join("harness_stdout.log");
        let stderr_path = trial_dir.join("harness_stderr.log");
        let stdout_ref = if stdout_path.exists() {
            Some(artifact_store.put_file(&stdout_path)?)
        } else {
            None
        };
        let stderr_ref = if stderr_path.exists() {
            Some(artifact_store.put_file(&stderr_path)?)
        } else {
            None
        };

        let hook_events_path = harness
            .events_path
            .as_ref()
            .map(|path| resolve_event_path(path, &trial_paths, container_mode))
            .filter(|path| path.exists());
        let hook_events_ref = if let Some(path) = hook_events_path.as_ref() {
            Some(artifact_store.put_file(path)?)
        } else {
            None
        };

        let trial_duration_ms = trial_started_at.elapsed().as_secs_f64() * 1000.0;

        let evidence_record = json!({
            "schema_version": "evidence_record_v1",
            "ts": Utc::now().to_rfc3339(),
            "ids": {
                "run_id": run_id.as_str(),
                "trial_id": trial_id.as_str(),
                "variant_id": variant.id.as_str(),
                "task_id": task_id.as_str(),
                "repl_idx": repl
            },
            "policy": {
                "state_policy": match effective_policy.state_policy {
                    StatePolicy::IsolatePerTrial => "isolate_per_trial",
                    StatePolicy::PersistPerTask => "persist_per_task",
                    StatePolicy::Accumulate => "accumulate",
                },
                "task_model": effective_policy.task_model.as_str(),
                "chain_id": chain_key.as_str(),
                "chain_step_index": chain_step_index
            },
            "runtime": {
                "executor": executor_kind.as_str(),
                "container_mode": container_mode,
                "exit_status": status.as_str(),
                "duration_ms": trial_duration_ms
            },
            "evidence": {
                "trial_input_ref": trial_input_ref.clone(),
                "trial_output_ref": trial_output_ref.clone(),
                "stdout_ref": stdout_ref.clone(),
                "stderr_ref": stderr_ref.clone(),
                "hook_events_ref": hook_events_ref.clone(),
                "harness_request_ref": trial_input_ref.clone(),
                "harness_response_ref": trial_output_ref.clone(),
                "workspace_pre_ref": pre_snapshot_ref.clone(),
                "workspace_post_ref": post_snapshot_ref.clone(),
                "diff_incremental_ref": diff_incremental_ref.clone(),
                "diff_cumulative_ref": diff_cumulative_ref.clone(),
                "patch_incremental_ref": patch_incremental_ref.clone(),
                "patch_cumulative_ref": patch_cumulative_ref.clone()
            },
            "paths": {
                "trial_dir": rel_to_run_dir(&trial_dir, &run_dir),
                "trial_input": rel_to_run_dir(&canonical_input_path, &run_dir),
                "trial_output": rel_to_run_dir(&canonical_output, &run_dir),
                "stdout": rel_to_run_dir(&stdout_path, &run_dir),
                "stderr": rel_to_run_dir(&stderr_path, &run_dir),
                "hook_events": hook_events_path.as_ref().map(|p| rel_to_run_dir(p, &run_dir)),
                "workspace_pre_snapshot": rel_to_run_dir(&pre_snapshot_path, &run_dir),
                "workspace_post_snapshot": rel_to_run_dir(&post_snapshot_path, &run_dir),
                "diff_incremental": rel_to_run_dir(&diff_incremental_path, &run_dir),
                "diff_cumulative": rel_to_run_dir(&diff_cumulative_path, &run_dir),
                "patch_incremental": rel_to_run_dir(&patch_incremental_path, &run_dir),
                "patch_cumulative": rel_to_run_dir(&patch_cumulative_path, &run_dir)
            }
        });

        validate_required_evidence_classes(
            &evidence_record,
            &effective_policy.required_evidence_classes,
        )?;
        append_jsonl(&evidence_records_path, &evidence_record)?;

        let chain_state_record = json!({
            "schema_version": "task_chain_state_v1",
            "ts": Utc::now().to_rfc3339(),
            "run_id": run_id.as_str(),
            "chain_id": chain_key.as_str(),
            "task_model": effective_policy.task_model.as_str(),
            "step_index": chain_step_index,
            "ids": {
                "trial_id": trial_id.as_str(),
                "variant_id": variant.id.as_str(),
                "task_id": task_id.as_str(),
                "repl_idx": repl
            },
            "snapshots": {
                "chain_root_ref": chain_root_snapshot_ref,
                "prev_ref": pre_snapshot_ref,
                "post_ref": post_snapshot_ref
            },
            "diffs": {
                "incremental_ref": diff_incremental_ref,
                "cumulative_ref": diff_cumulative_ref,
                "patch_incremental_ref": patch_incremental_ref,
                "patch_cumulative_ref": patch_cumulative_ref
            },
            "ext": {
                "chain_fs_key": chain_fs_key,
                "latest_snapshot_ref": chain_states
                    .get(&chain_key)
                    .map(|state| state.latest_snapshot_ref.clone())
            }
        });
        append_jsonl(&task_chain_states_path, &chain_state_record)?;

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
            .map(|(action, requested_by, _)| action == "stop" && requested_by == "lab_pause")
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
            break 'schedule;
        } else if status == "0" && outcome != "error" {
            trial_guard.complete("completed", None)?;
            *consecutive_failures.entry(slot.variant_idx).or_default() = 0;
        } else if status != "0" {
            trial_guard.complete("failed", Some("harness_exit_nonzero"))?;
            *consecutive_failures.entry(slot.variant_idx).or_default() += 1;
        } else {
            trial_guard.complete("failed", Some("trial_output_error"))?;
            *consecutive_failures.entry(slot.variant_idx).or_default() += 1;
        }

        // Pruning check
        if let Some(max_failures) = policy_config.pruning_max_consecutive_failures {
            let count = consecutive_failures
                .get(&slot.variant_idx)
                .copied()
                .unwrap_or(0);
            if count >= max_failures {
                pruned_variants.insert(slot.variant_idx);
            }
        }

        write_run_control(&run_dir, &run_id, "running", None, None)?;
        apply_materialization_policy(&trial_dir, materialize_mode)?;
    }

    validate_jsonl_against_schema("evidence_record_v1.jsonschema", &evidence_records_path)?;
    validate_jsonl_against_schema("task_chain_state_v1.jsonschema", &task_chain_states_path)?;

    let benchmark_artifacts = process_benchmark_outputs(
        &project_root,
        &run_dir,
        &run_id,
        &trial_summaries,
        &benchmark_config,
        &evidence_records_path,
        &task_chain_states_path,
    )?;

    apply_score_records_to_trial_summaries(&mut trial_summaries, &benchmark_artifacts.scores_path)?;

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
        "isolation_grade": if container_mode {"bounded"} else {"leaky"},
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

    let policy_config = parse_policies(&json_value);
    let comparison = json_value
        .pointer("/design/comparison")
        .and_then(|v| v.as_str())
        .unwrap_or("paired")
        .to_string();

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
        scheduling: match policy_config.scheduling {
            SchedulingPolicy::PairedInterleaved => "paired_interleaved".to_string(),
            SchedulingPolicy::VariantSequential => "variant_sequential".to_string(),
            SchedulingPolicy::Randomized => "randomized".to_string(),
        },
        state_policy: match policy_config.state {
            StatePolicy::IsolatePerTrial => "isolate_per_trial".to_string(),
            StatePolicy::PersistPerTask => "persist_per_task".to_string(),
            StatePolicy::Accumulate => "accumulate".to_string(),
        },
        comparison,
        retry_max_attempts: policy_config.retry_max_attempts,
    })
}

// ---------------------------------------------------------------------------
// Trial scheduling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulingPolicy {
    PairedInterleaved,
    VariantSequential,
    Randomized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatePolicy {
    IsolatePerTrial,
    PersistPerTask,
    Accumulate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskModel {
    Independent,
    Dependent,
}

impl TaskModel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Independent => "independent",
            Self::Dependent => "dependent",
        }
    }
}

#[derive(Debug, Clone)]
struct BenchmarkPolicyConfig {
    task_model: TaskModel,
    scoring_lifecycle: String,
    evaluator_mode: String,
    required_evidence_classes: Vec<String>,
    chain_failure_policy: String,
}

impl Default for BenchmarkPolicyConfig {
    fn default() -> Self {
        Self {
            task_model: TaskModel::Independent,
            scoring_lifecycle: "predict_then_score".to_string(),
            evaluator_mode: "custom".to_string(),
            required_evidence_classes: Vec::new(),
            chain_failure_policy: "continue_with_flag".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct BenchmarkAdapterConfig {
    command: Vec<String>,
    manifest: Option<Value>,
}

#[derive(Debug, Clone, Default)]
struct BenchmarkConfig {
    policy: BenchmarkPolicyConfig,
    adapter: Option<BenchmarkAdapterConfig>,
}

#[derive(Debug, Clone)]
struct EffectiveTaskPolicy {
    state_policy: StatePolicy,
    task_model: TaskModel,
    scoring_lifecycle: String,
    required_evidence_classes: Vec<String>,
    chain_failure_policy: String,
}

#[derive(Debug, Clone)]
struct ChainRuntimeState {
    chain_root_snapshot_ref: String,
    chain_root_snapshot_path: PathBuf,
    latest_snapshot_ref: String,
    latest_snapshot_path: PathBuf,
    step_index: usize,
}

#[derive(Debug, Clone)]
struct PolicyConfig {
    scheduling: SchedulingPolicy,
    state: StatePolicy,
    retry_max_attempts: usize,
    retry_on: Vec<String>,
    pruning_max_consecutive_failures: Option<usize>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            scheduling: SchedulingPolicy::VariantSequential,
            state: StatePolicy::IsolatePerTrial,
            retry_max_attempts: 1,
            retry_on: vec![],
            pruning_max_consecutive_failures: None,
        }
    }
}

fn parse_policies(json_value: &Value) -> PolicyConfig {
    let policies = json_value.pointer("/design/policies");
    let Some(p) = policies else {
        return PolicyConfig::default();
    };

    let scheduling = match p.pointer("/scheduling").and_then(|v| v.as_str()) {
        Some("paired_interleaved") => SchedulingPolicy::PairedInterleaved,
        Some("randomized") => SchedulingPolicy::Randomized,
        _ => SchedulingPolicy::VariantSequential,
    };
    let state = match p.pointer("/state").and_then(|v| v.as_str()) {
        Some("persist_per_task") => StatePolicy::PersistPerTask,
        Some("accumulate") => StatePolicy::Accumulate,
        _ => StatePolicy::IsolatePerTrial,
    };
    let retry_max_attempts = p
        .pointer("/retry/max_attempts")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as usize;
    let retry_on = p
        .pointer("/retry/retry_on")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let pruning_max_consecutive_failures = p
        .pointer("/pruning/max_consecutive_failures")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    PolicyConfig {
        scheduling,
        state,
        retry_max_attempts,
        retry_on,
        pruning_max_consecutive_failures,
    }
}

fn parse_task_model(value: Option<&str>) -> TaskModel {
    match value {
        Some("dependent") => TaskModel::Dependent,
        _ => TaskModel::Independent,
    }
}

fn parse_state_policy_value(value: Option<&str>) -> Option<StatePolicy> {
    match value {
        Some("isolate_per_trial") => Some(StatePolicy::IsolatePerTrial),
        Some("persist_per_task") => Some(StatePolicy::PersistPerTask),
        Some("accumulate") => Some(StatePolicy::Accumulate),
        _ => None,
    }
}

fn parse_benchmark_config(json_value: &Value) -> BenchmarkConfig {
    let benchmark_root = json_value.pointer("/benchmark");
    let Some(root) = benchmark_root else {
        return BenchmarkConfig::default();
    };

    let policy = root.pointer("/policy");
    let mut policy_config = BenchmarkPolicyConfig::default();
    if let Some(p) = policy {
        policy_config.task_model = parse_task_model(p.pointer("/task_model").and_then(|v| v.as_str()));
        if let Some(v) = p.pointer("/scoring_lifecycle").and_then(|v| v.as_str()) {
            policy_config.scoring_lifecycle = v.to_string();
        }
        if let Some(v) = p.pointer("/evaluator_mode").and_then(|v| v.as_str()) {
            policy_config.evaluator_mode = v.to_string();
        }
        if let Some(v) = p.pointer("/chain_failure_policy").and_then(|v| v.as_str()) {
            policy_config.chain_failure_policy = v.to_string();
        }
        if let Some(arr) = p
            .pointer("/required_evidence_classes")
            .and_then(|v| v.as_array())
        {
            policy_config.required_evidence_classes = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
    }

    let adapter = root.pointer("/adapter").and_then(|a| {
        let command = a
            .pointer("/command")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if command.is_empty() {
            return None;
        }
        let manifest = a.pointer("/manifest").cloned();
        Some(BenchmarkAdapterConfig { command, manifest })
    });

    BenchmarkConfig {
        policy: policy_config,
        adapter,
    }
}

fn resolve_effective_task_policy(
    experiment_policy: &PolicyConfig,
    benchmark_policy: &BenchmarkPolicyConfig,
    task_payload: &Value,
) -> EffectiveTaskPolicy {
    let override_obj = task_payload
        .get("policy_override")
        .and_then(|v| v.as_object());

    let state_override = override_obj
        .and_then(|o| o.get("state_policy"))
        .and_then(|v| v.as_str())
        .and_then(|s| parse_state_policy_value(Some(s)));
    let task_model_override = override_obj
        .and_then(|o| o.get("task_model"))
        .and_then(|v| v.as_str())
        .map(|s| parse_task_model(Some(s)));
    let scoring_lifecycle_override = override_obj
        .and_then(|o| o.get("scoring_lifecycle"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let chain_failure_override = override_obj
        .and_then(|o| o.get("chain_failure_policy"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let required_evidence_override = override_obj
        .and_then(|o| o.get("required_evidence_classes"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });

    EffectiveTaskPolicy {
        state_policy: state_override.unwrap_or(experiment_policy.state),
        task_model: task_model_override.unwrap_or(benchmark_policy.task_model),
        scoring_lifecycle: scoring_lifecycle_override
            .unwrap_or_else(|| benchmark_policy.scoring_lifecycle.clone()),
        required_evidence_classes: required_evidence_override
            .unwrap_or_else(|| benchmark_policy.required_evidence_classes.clone()),
        chain_failure_policy: chain_failure_override
            .unwrap_or_else(|| benchmark_policy.chain_failure_policy.clone()),
    }
}

fn validate_required_evidence_classes(record: &Value, required: &[String]) -> Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    for class_name in required {
        let pointer = format!("/evidence/{}", class_name);
        let value = record.pointer(&pointer);
        let missing = match value {
            None => true,
            Some(Value::Null) => true,
            Some(Value::String(s)) => s.trim().is_empty(),
            _ => false,
        };
        if missing {
            return Err(anyhow!(
                "missing required evidence class '{}'; pointer {}",
                class_name,
                pointer
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct BenchmarkArtifactsPaths {
    scores_path: PathBuf,
}

fn normalize_benchmark_manifest(
    run_id: &str,
    manifest: Option<Value>,
    policy: &BenchmarkPolicyConfig,
) -> Value {
    let mut normalized = manifest.unwrap_or_else(|| json!({}));
    if !normalized.is_object() {
        normalized = json!({});
    }
    let obj = normalized.as_object_mut().expect("manifest object");

    obj.entry("schema_version".to_string())
        .or_insert_with(|| json!("benchmark_adapter_manifest_v1"));
    obj.entry("created_at".to_string())
        .or_insert_with(|| json!(Utc::now().to_rfc3339()));
    obj.entry("adapter_id".to_string())
        .or_insert_with(|| json!("runner_passthrough"));
    obj.entry("adapter_version".to_string())
        .or_insert_with(|| json!("0.1.0"));

    if !obj.contains_key("benchmark") {
        obj.insert(
            "benchmark".to_string(),
            json!({
                "name": "unspecified_benchmark",
                "version": "unknown",
                "split": "unknown"
            }),
        );
    } else if let Some(benchmark_obj) = obj.get_mut("benchmark").and_then(|v| v.as_object_mut()) {
        benchmark_obj
            .entry("name".to_string())
            .or_insert_with(|| json!("unspecified_benchmark"));
        benchmark_obj
            .entry("split".to_string())
            .or_insert_with(|| json!("unknown"));
    }

    obj.entry("execution_mode".to_string())
        .or_insert_with(|| json!(policy.scoring_lifecycle.clone()));
    obj.entry("record_schemas".to_string()).or_insert_with(|| {
        json!({
            "prediction": "benchmark_prediction_record_v1",
            "score": "benchmark_score_record_v1"
        })
    });
    obj.entry("evaluator".to_string()).or_insert_with(|| {
        json!({
            "name": "runner_passthrough",
            "version": "0.1.0",
            "mode": policy.evaluator_mode
        })
    });
    obj.entry("ext".to_string())
        .or_insert_with(|| json!({"run_id": run_id}));

    normalized
}

fn benchmark_identity_from_manifest(manifest: &Value) -> (String, String, Option<String>, String) {
    let adapter_id = manifest
        .pointer("/adapter_id")
        .and_then(|v| v.as_str())
        .unwrap_or("runner_passthrough")
        .to_string();
    let name = manifest
        .pointer("/benchmark/name")
        .and_then(|v| v.as_str())
        .unwrap_or("unspecified_benchmark")
        .to_string();
    let version = manifest
        .pointer("/benchmark/version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let split = manifest
        .pointer("/benchmark/split")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    (adapter_id, name, version, split)
}

fn read_jsonl_records(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)?;
    let mut rows = Vec::new();
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(serde_json::from_str::<Value>(line)?);
    }
    Ok(rows)
}

fn write_jsonl_records(path: &Path, rows: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut file = fs::File::create(path)?;
    for row in rows {
        serde_json::to_writer(&mut file, row)?;
        writeln!(&mut file)?;
    }
    Ok(())
}

fn validate_json_file_against_schema(schema_name: &str, path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!(
            "required artifact missing for schema {}: {}",
            schema_name,
            path.display()
        ));
    }
    let schema = compile_schema(schema_name)?;
    let raw = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&raw)?;
    if let Err(errors) = schema.validate(&value) {
        let msgs = errors.map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(anyhow!(
            "schema validation failed ({}) {}: {}",
            schema_name,
            path.display(),
            msgs
        ));
    }
    Ok(())
}

fn validate_jsonl_against_schema(schema_name: &str, path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!(
            "required artifact missing for schema {}: {}",
            schema_name,
            path.display()
        ));
    }
    let schema = compile_schema(schema_name)?;
    let data = fs::read_to_string(path)?;
    for (idx, line) in data.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            anyhow!(
                "invalid json line {} in {}: {}",
                idx + 1,
                path.display(),
                e
            )
        })?;
        match schema.validate(&value) {
            Ok(_) => {}
            Err(errors) => {
                let msgs = errors.map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
                return Err(anyhow!(
                    "schema validation failed ({}) {} line {}: {}",
                    schema_name,
                    path.display(),
                    idx + 1,
                    msgs
                ));
            }
        };
    }
    Ok(())
}

fn verdict_from_outcome(outcome: &str) -> &'static str {
    match outcome {
        "success" => "pass",
        "missing" => "missing",
        "error" => "error",
        _ => "fail",
    }
}

fn outcome_from_verdict(verdict: &str) -> &'static str {
    match verdict {
        "pass" => "success",
        "missing" => "missing",
        "error" => "error",
        _ => "failure",
    }
}

fn build_benchmark_summary(run_id: &str, manifest: &Value, score_rows: &[Value]) -> Value {
    let (adapter_id, name, version, split) = benchmark_identity_from_manifest(manifest);
    let evaluator = manifest
        .pointer("/evaluator")
        .cloned()
        .unwrap_or_else(|| json!({"name": "runner_passthrough", "mode": "custom"}));

    let mut totals = BTreeMap::from([
        ("pass".to_string(), 0usize),
        ("fail".to_string(), 0usize),
        ("missing".to_string(), 0usize),
        ("error".to_string(), 0usize),
    ]);
    let mut by_variant: BTreeMap<String, Vec<&Value>> = BTreeMap::new();

    for row in score_rows {
        let verdict = row
            .pointer("/verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("error")
            .to_string();
        *totals.entry(verdict).or_default() += 1;
        let variant_id = row
            .pointer("/ids/variant_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        by_variant.entry(variant_id).or_default().push(row);
    }

    let mut variants = Vec::new();
    for (variant_id, rows) in by_variant {
        let total = rows.len();
        let pass = rows
            .iter()
            .filter(|r| r.pointer("/verdict").and_then(|v| v.as_str()) == Some("pass"))
            .count();
        let fail = rows
            .iter()
            .filter(|r| r.pointer("/verdict").and_then(|v| v.as_str()) == Some("fail"))
            .count();
        let missing = rows
            .iter()
            .filter(|r| r.pointer("/verdict").and_then(|v| v.as_str()) == Some("missing"))
            .count();
        let error = rows
            .iter()
            .filter(|r| r.pointer("/verdict").and_then(|v| v.as_str()) == Some("error"))
            .count();
        let pass_rate = if total > 0 {
            pass as f64 / total as f64
        } else {
            0.0
        };
        let primary_metric_name = rows
            .iter()
            .find_map(|r| {
                r.pointer("/primary_metric_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "resolved".to_string());
        let mut pm_sum = 0.0f64;
        let mut pm_count = 0usize;
        for row in rows {
            if let Some(v) = row
                .pointer("/primary_metric_value")
                .and_then(|v| v.as_f64())
            {
                pm_sum += v;
                pm_count += 1;
            }
        }
        let primary_metric_mean = if pm_count > 0 {
            pm_sum / pm_count as f64
        } else {
            0.0
        };
        variants.push(json!({
            "variant_id": variant_id,
            "total": total,
            "pass": pass,
            "fail": fail,
            "missing": missing,
            "error": error,
            "pass_rate": pass_rate,
            "primary_metric_name": primary_metric_name,
            "primary_metric_mean": primary_metric_mean
        }));
    }

    json!({
        "schema_version": "benchmark_summary_v1",
        "created_at": Utc::now().to_rfc3339(),
        "run_id": run_id,
        "benchmark": {
            "adapter_id": adapter_id,
            "name": name,
            "version": version,
            "split": split
        },
        "evaluator": evaluator,
        "totals": {
            "trials": score_rows.len(),
            "pass": totals.get("pass").copied().unwrap_or(0),
            "fail": totals.get("fail").copied().unwrap_or(0),
            "missing": totals.get("missing").copied().unwrap_or(0),
            "error": totals.get("error").copied().unwrap_or(0)
        },
        "variants": variants
    })
}

fn generate_passthrough_benchmark_records(
    run_id: &str,
    manifest: &Value,
    trial_summaries: &[Value],
    predictions_path: &Path,
    scores_path: &Path,
    summary_path: &Path,
) -> Result<()> {
    let (adapter_id, name, version, split) = benchmark_identity_from_manifest(manifest);
    let evaluator = manifest
        .pointer("/evaluator")
        .cloned()
        .unwrap_or_else(|| json!({"name": "runner_passthrough", "mode": "custom"}));

    let mut prediction_rows = Vec::new();
    let mut score_rows = Vec::new();
    for summary in trial_summaries {
        let ids = json!({
            "run_id": summary.pointer("/run_id").and_then(|v| v.as_str()).unwrap_or(run_id),
            "trial_id": summary.pointer("/trial_id").and_then(|v| v.as_str()).unwrap_or(""),
            "variant_id": summary.pointer("/variant_id").and_then(|v| v.as_str()).unwrap_or(""),
            "task_id": summary.pointer("/task_id").and_then(|v| v.as_str()).unwrap_or(""),
            "repl_idx": summary.pointer("/repl_idx").and_then(|v| v.as_u64()).unwrap_or(0),
        });
        let outcome = summary
            .pointer("/outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("error");
        let verdict = verdict_from_outcome(outcome);
        let primary_metric_name = summary
            .pointer("/primary_metric_name")
            .and_then(|v| v.as_str())
            .unwrap_or("resolved")
            .to_string();
        let primary_metric_value = summary
            .pointer("/primary_metric_value")
            .and_then(|v| v.as_f64())
            .unwrap_or(if verdict == "pass" { 1.0 } else { 0.0 });

        prediction_rows.push(json!({
            "schema_version": "benchmark_prediction_record_v1",
            "ts": Utc::now().to_rfc3339(),
            "ids": ids,
            "benchmark": {
                "adapter_id": adapter_id.clone(),
                "name": name.clone(),
                "version": version.clone(),
                "split": split.clone()
            },
            "prediction": {
                "kind": "json",
                "value": {
                    "outcome": outcome,
                    "metrics": summary.pointer("/metrics").cloned().unwrap_or(json!({}))
                }
            },
            "metrics": summary.pointer("/metrics").cloned().unwrap_or(json!({}))
        }));

        score_rows.push(json!({
            "schema_version": "benchmark_score_record_v1",
            "ts": Utc::now().to_rfc3339(),
            "ids": ids,
            "benchmark": {
                "adapter_id": adapter_id.clone(),
                "name": name.clone(),
                "version": version.clone(),
                "split": split.clone()
            },
            "verdict": verdict,
            "primary_metric_name": primary_metric_name,
            "primary_metric_value": primary_metric_value,
            "metrics": summary.pointer("/metrics").cloned().unwrap_or(json!({})),
            "evaluator": evaluator.clone()
        }));
    }

    write_jsonl_records(predictions_path, &prediction_rows)?;
    write_jsonl_records(scores_path, &score_rows)?;
    let summary = build_benchmark_summary(run_id, manifest, &score_rows);
    atomic_write_json_pretty(summary_path, &summary)?;
    Ok(())
}

fn process_benchmark_outputs(
    project_root: &Path,
    run_dir: &Path,
    run_id: &str,
    trial_summaries: &[Value],
    benchmark_config: &BenchmarkConfig,
    evidence_records_path: &Path,
    task_chain_states_path: &Path,
) -> Result<BenchmarkArtifactsPaths> {
    let benchmark_dir = run_dir.join("benchmark");
    ensure_dir(&benchmark_dir)?;
    let manifest_path = benchmark_dir.join("adapter_manifest.json");
    let predictions_path = benchmark_dir.join("predictions.jsonl");
    let scores_path = benchmark_dir.join("scores.jsonl");
    let summary_path = benchmark_dir.join("summary.json");

    let manifest = normalize_benchmark_manifest(
        run_id,
        benchmark_config
            .adapter
            .as_ref()
            .and_then(|a| a.manifest.clone()),
        &benchmark_config.policy,
    );
    atomic_write_json_pretty(&manifest_path, &manifest)?;

    if let Some(adapter) = benchmark_config.adapter.as_ref() {
        if adapter.command.is_empty() {
            return Err(anyhow!("benchmark adapter command cannot be empty"));
        }
        let mut cmd = Command::new(&adapter.command[0]);
        cmd.args(&adapter.command[1..]);
        cmd.current_dir(project_root);
        cmd.env("AGENTLAB_RUN_ID", run_id);
        cmd.env("AGENTLAB_RUN_DIR", run_dir);
        cmd.env("AGENTLAB_EVIDENCE_RECORDS_PATH", evidence_records_path);
        cmd.env("AGENTLAB_TASK_CHAIN_STATES_PATH", task_chain_states_path);
        cmd.env("AGENTLAB_BENCHMARK_DIR", &benchmark_dir);
        cmd.env("AGENTLAB_ADAPTER_MANIFEST_PATH", &manifest_path);
        cmd.env("AGENTLAB_PREDICTIONS_PATH", &predictions_path);
        cmd.env("AGENTLAB_SCORES_PATH", &scores_path);
        cmd.env("AGENTLAB_BENCHMARK_SUMMARY_PATH", &summary_path);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        let status = cmd.status()?;
        if !status.success() {
            return Err(anyhow!(
                "benchmark adapter command failed with status {}",
                status
            ));
        }
        if !predictions_path.exists() {
            return Err(anyhow!(
                "benchmark adapter did not produce predictions.jsonl"
            ));
        }
        if !scores_path.exists() {
            return Err(anyhow!("benchmark adapter did not produce scores.jsonl"));
        }
        if !summary_path.exists() {
            let scores = read_jsonl_records(&scores_path)?;
            let summary = build_benchmark_summary(run_id, &manifest, &scores);
            atomic_write_json_pretty(&summary_path, &summary)?;
        }
    } else {
        generate_passthrough_benchmark_records(
            run_id,
            &manifest,
            trial_summaries,
            &predictions_path,
            &scores_path,
            &summary_path,
        )?;
    }

    validate_json_file_against_schema("benchmark_adapter_manifest_v1.jsonschema", &manifest_path)?;
    validate_jsonl_against_schema("benchmark_prediction_record_v1.jsonschema", &predictions_path)?;
    validate_jsonl_against_schema("benchmark_score_record_v1.jsonschema", &scores_path)?;
    validate_json_file_against_schema("benchmark_summary_v1.jsonschema", &summary_path)?;

    Ok(BenchmarkArtifactsPaths { scores_path })
}

fn apply_score_records_to_trial_summaries(
    trial_summaries: &mut [Value],
    scores_path: &Path,
) -> Result<()> {
    if !scores_path.exists() {
        return Ok(());
    }
    let scores = read_jsonl_records(scores_path)?;
    if scores.is_empty() {
        return Ok(());
    }
    let mut by_trial: BTreeMap<String, &Value> = BTreeMap::new();
    for score in &scores {
        if let Some(trial_id) = score
            .pointer("/ids/trial_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            by_trial.insert(trial_id, score);
        }
    }

    for summary in trial_summaries.iter_mut() {
        let trial_id = summary
            .pointer("/trial_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(score) = by_trial.get(trial_id) else {
            continue;
        };
        let verdict = score
            .pointer("/verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("error");
        let mapped_outcome = outcome_from_verdict(verdict);
        if let Some(obj) = summary.as_object_mut() {
            obj.insert("outcome".to_string(), json!(mapped_outcome));
            obj.insert("success".to_string(), json!(verdict == "pass"));
            if let Some(name) = score.pointer("/primary_metric_name").and_then(|v| v.as_str()) {
                obj.insert("primary_metric_name".to_string(), json!(name));
            }
            if let Some(value) = score.pointer("/primary_metric_value") {
                obj.insert("primary_metric_value".to_string(), value.clone());
            }
            let mut metrics = obj
                .get("metrics")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if let Some(metrics_obj) = metrics.as_object_mut() {
                metrics_obj.insert("benchmark_verdict".to_string(), json!(verdict));
            }
            obj.insert("metrics".to_string(), metrics);
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct TrialSlot {
    variant_idx: usize,
    task_idx: usize,
    repl_idx: usize,
}

fn build_trial_schedule(
    variant_count: usize,
    task_count: usize,
    replications: usize,
    policy: SchedulingPolicy,
    random_seed: u64,
) -> Vec<TrialSlot> {
    let mut slots = Vec::with_capacity(variant_count * task_count * replications);

    match policy {
        SchedulingPolicy::VariantSequential => {
            for v in 0..variant_count {
                for t in 0..task_count {
                    for r in 0..replications {
                        slots.push(TrialSlot {
                            variant_idx: v,
                            task_idx: t,
                            repl_idx: r,
                        });
                    }
                }
            }
        }
        SchedulingPolicy::PairedInterleaved => {
            for t in 0..task_count {
                for v in 0..variant_count {
                    for r in 0..replications {
                        slots.push(TrialSlot {
                            variant_idx: v,
                            task_idx: t,
                            repl_idx: r,
                        });
                    }
                }
            }
        }
        SchedulingPolicy::Randomized => {
            // Build variant_sequential order then shuffle deterministically
            for v in 0..variant_count {
                for t in 0..task_count {
                    for r in 0..replications {
                        slots.push(TrialSlot {
                            variant_idx: v,
                            task_idx: t,
                            repl_idx: r,
                        });
                    }
                }
            }
            // Deterministic Fisher-Yates using LCG seeded by random_seed
            let mut rng_state: u64 = random_seed;
            for i in (1..slots.len()).rev() {
                // LCG: state = state * 6364136223846793005 + 1442695040888963407
                rng_state = rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let j = (rng_state >> 33) as usize % (i + 1);
                slots.swap(i, j);
            }
        }
    }

    slots
}

fn should_retry_outcome(outcome: &str, exit_status: &str, retry_on: &[String]) -> bool {
    if retry_on.is_empty() {
        // When retry_on is unspecified, retry on any non-success
        return outcome == "error" || exit_status != "0";
    }
    for trigger in retry_on {
        match trigger.as_str() {
            "error" if outcome == "error" => return true,
            "failure" if exit_status != "0" => return true,
            "timeout" if outcome == "timeout" => return true,
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------------------

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

const TASK_BOUNDARY_V1_SCHEMA_VERSION: &str = "task_boundary_v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceFileSpec {
    path: String,
    content: String,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MountReferenceSpec {
    dataset_pack_ref: String,
    mount_path: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TaskBoundaryLimits {
    #[serde(default)]
    max_steps: Option<u64>,
    #[serde(default)]
    max_total_tokens: Option<u64>,
    #[serde(default)]
    max_tool_calls: Option<u64>,
    #[serde(default)]
    trial_seconds: Option<u64>,
}

impl TaskBoundaryLimits {
    fn is_empty(&self) -> bool {
        self.max_steps.is_none()
            && self.max_total_tokens.is_none()
            && self.max_tool_calls.is_none()
            && self.trial_seconds.is_none()
    }
}

#[derive(Debug, Clone)]
struct TaskBoundaryMaterialization {
    task_payload: Value,
    workspace_files: Vec<WorkspaceFileSpec>,
    mount_references: Vec<MountReferenceSpec>,
    limits: TaskBoundaryLimits,
}

#[derive(Debug, Clone)]
struct ResolvedMountReference {
    host_path: PathBuf,
    mount_path: String,
}

fn default_task_boundary(task_payload: Value) -> TaskBoundaryMaterialization {
    TaskBoundaryMaterialization {
        task_payload,
        workspace_files: Vec::new(),
        mount_references: Vec::new(),
        limits: TaskBoundaryLimits::default(),
    }
}

fn parse_task_boundary_from_dataset_task(task: &Value) -> Result<TaskBoundaryMaterialization> {
    if task.get("schema_version").and_then(|v| v.as_str()) != Some(TASK_BOUNDARY_V1_SCHEMA_VERSION)
    {
        return Ok(default_task_boundary(task.clone()));
    }
    let obj = task
        .as_object()
        .ok_or_else(|| anyhow!("task boundary must be an object"))?;

    let allowed = [
        "schema_version",
        "task",
        "workspace_files",
        "mount_references",
        "limits",
    ];
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(anyhow!(
                "task boundary contains unsupported key '{}'; expected task + workspace_files + mount_references + limits",
                key
            ));
        }
    }

    let task_payload = obj
        .get("task")
        .cloned()
        .ok_or_else(|| anyhow!("task boundary missing field: task"))?;
    if !task_payload.is_object() {
        return Err(anyhow!("task boundary field 'task' must be an object"));
    }

    Ok(TaskBoundaryMaterialization {
        task_payload,
        workspace_files: parse_workspace_files(obj.get("workspace_files"))?,
        mount_references: parse_mount_references(obj.get("mount_references"))?,
        limits: parse_task_limits(obj.get("limits"))?,
    })
}

fn parse_task_boundary_from_trial_input(input: &Value) -> Result<TaskBoundaryMaterialization> {
    // Backward compatibility: older trial_input fixtures may not include /task.
    let task_payload = input
        .pointer("/task")
        .cloned()
        .or_else(|| input.pointer("/dataset/task").cloned())
        .unwrap_or_else(|| json!({}));
    if !task_payload.is_object() {
        return Err(anyhow!("trial_input task payload must be an object"));
    }

    if let Some(ext) = input.pointer("/ext/task_boundary_v1") {
        parse_task_boundary_ext(ext, task_payload)
    } else if task_payload.get("schema_version").and_then(|v| v.as_str())
        == Some(TASK_BOUNDARY_V1_SCHEMA_VERSION)
    {
        parse_task_boundary_from_dataset_task(&task_payload)
    } else {
        Ok(default_task_boundary(task_payload))
    }
}

fn parse_task_boundary_ext(
    ext: &Value,
    task_payload: Value,
) -> Result<TaskBoundaryMaterialization> {
    let obj = ext
        .as_object()
        .ok_or_else(|| anyhow!("trial_input /ext/task_boundary_v1 must be an object"))?;
    if let Some(schema_version) = obj.get("schema_version") {
        if schema_version.as_str() != Some(TASK_BOUNDARY_V1_SCHEMA_VERSION) {
            return Err(anyhow!(
                "unsupported task boundary schema version in /ext/task_boundary_v1"
            ));
        }
    }

    Ok(TaskBoundaryMaterialization {
        task_payload,
        workspace_files: parse_workspace_files(obj.get("workspace_files"))?,
        mount_references: parse_mount_references(obj.get("mount_references"))?,
        limits: parse_task_limits(obj.get("limits"))?,
    })
}

fn parse_workspace_files(value: Option<&Value>) -> Result<Vec<WorkspaceFileSpec>> {
    let Some(raw) = value else {
        return Ok(Vec::new());
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| anyhow!("task boundary workspace_files must be an array"))?;

    let mut files = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let file: WorkspaceFileSpec = serde_json::from_value(item.clone())
            .map_err(|e| anyhow!("invalid workspace_files[{}]: {}", idx, e))?;
        let _ = validate_workspace_relative_path(&file.path).map_err(|e| {
            anyhow!(
                "invalid workspace_files[{}].path '{}': {}",
                idx,
                file.path,
                e
            )
        })?;
        if let Some(encoding) = file.encoding.as_deref() {
            if encoding != "utf8" && encoding != "base64" {
                return Err(anyhow!(
                    "workspace_files[{}].encoding must be 'utf8' or 'base64'",
                    idx
                ));
            }
        }
        files.push(file);
    }
    Ok(files)
}

fn parse_mount_references(value: Option<&Value>) -> Result<Vec<MountReferenceSpec>> {
    let Some(raw) = value else {
        return Ok(Vec::new());
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| anyhow!("task boundary mount_references must be an array"))?;

    let mut mounts = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let mount: MountReferenceSpec = serde_json::from_value(item.clone())
            .map_err(|e| anyhow!("invalid mount_references[{}]: {}", idx, e))?;
        if !mount.read_only {
            return Err(anyhow!("mount_references[{}].read_only must be true", idx));
        }
        validate_container_workspace_path(&mount.mount_path).map_err(|e| {
            anyhow!(
                "invalid mount_references[{}].mount_path '{}': {}",
                idx,
                mount.mount_path,
                e
            )
        })?;
        let _ = parse_dataset_pack_ref_digest(&mount.dataset_pack_ref).map_err(|e| {
            anyhow!(
                "invalid mount_references[{}].dataset_pack_ref '{}': {}",
                idx,
                mount.dataset_pack_ref,
                e
            )
        })?;
        mounts.push(mount);
    }
    Ok(mounts)
}

fn parse_task_limits(value: Option<&Value>) -> Result<TaskBoundaryLimits> {
    let Some(raw) = value else {
        return Ok(TaskBoundaryLimits::default());
    };
    let limits: TaskBoundaryLimits =
        serde_json::from_value(raw.clone()).map_err(|e| anyhow!("invalid limits: {}", e))?;
    validate_limit_positive("max_steps", limits.max_steps)?;
    validate_limit_positive("max_total_tokens", limits.max_total_tokens)?;
    validate_limit_positive("max_tool_calls", limits.max_tool_calls)?;
    validate_limit_positive("trial_seconds", limits.trial_seconds)?;
    Ok(limits)
}

fn validate_limit_positive(name: &str, value: Option<u64>) -> Result<()> {
    if value == Some(0) {
        return Err(anyhow!("{} must be > 0 when provided", name));
    }
    Ok(())
}

fn validate_workspace_relative_path(path: &str) -> Result<PathBuf> {
    if path.trim().is_empty() {
        return Err(anyhow!("path cannot be empty"));
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(anyhow!("path must be relative to /workspace"));
    }
    let mut normalized = PathBuf::new();
    for component in p.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(seg) => normalized.push(seg),
            Component::ParentDir => {
                return Err(anyhow!("path cannot contain '..'"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("path cannot be absolute"));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(anyhow!("path cannot resolve to empty"));
    }
    Ok(normalized)
}

fn validate_container_workspace_path(path: &str) -> Result<()> {
    if !(path == "/workspace" || path.starts_with("/workspace/")) {
        return Err(anyhow!("mount_path must be under /workspace"));
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(anyhow!("mount_path must be absolute"));
    }
    for component in p.components() {
        if matches!(component, Component::ParentDir) {
            return Err(anyhow!("mount_path cannot contain '..'"));
        }
    }
    Ok(())
}

fn parse_dataset_pack_ref_digest(dataset_pack_ref: &str) -> Result<String> {
    let digest = dataset_pack_ref
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("dataset_pack_ref must start with 'sha256:'"))?;
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("dataset_pack_ref digest must be 64 hex characters"));
    }
    Ok(digest.to_ascii_lowercase())
}

fn resolve_dataset_pack_host_path(project_root: &Path, dataset_pack_ref: &str) -> Result<PathBuf> {
    let digest = parse_dataset_pack_ref_digest(dataset_pack_ref)?;
    let path = project_root
        .join(".lab")
        .join("dataset_packs")
        .join("sha256")
        .join(digest);
    if !path.exists() {
        return Err(anyhow!("dataset pack not found: {}", path.display()));
    }
    Ok(path)
}

fn resolve_task_mounts(
    project_root: &Path,
    mount_references: &[MountReferenceSpec],
    container_mode: bool,
) -> Result<Vec<ResolvedMountReference>> {
    if mount_references.is_empty() {
        return Ok(Vec::new());
    }
    if !container_mode {
        return Err(anyhow!("task mount_references require container executor"));
    }
    let mut mounts = Vec::with_capacity(mount_references.len());
    for mount in mount_references {
        let host_path = resolve_dataset_pack_host_path(project_root, &mount.dataset_pack_ref)?;
        mounts.push(ResolvedMountReference {
            host_path,
            mount_path: mount.mount_path.clone(),
        });
    }
    Ok(mounts)
}

fn materialize_workspace_files(
    paths: &TrialPaths,
    workspace_files: &[WorkspaceFileSpec],
) -> Result<()> {
    for file in workspace_files {
        let rel = validate_workspace_relative_path(&file.path)?;
        let host_path = paths.workspace.join(rel);
        let bytes = match file.encoding.as_deref() {
            None | Some("utf8") => file.content.as_bytes().to_vec(),
            Some("base64") => BASE64_STANDARD
                .decode(file.content.as_bytes())
                .map_err(|e| {
                    anyhow!(
                        "failed to decode base64 workspace file '{}': {}",
                        file.path,
                        e
                    )
                })?,
            Some(other) => {
                return Err(anyhow!(
                    "unsupported workspace file encoding '{}' for '{}'",
                    other,
                    file.path
                ));
            }
        };
        atomic_write_bytes(&host_path, &bytes)?;
        #[cfg(unix)]
        if file.executable {
            let metadata = fs::metadata(&host_path)?;
            let mut perms = metadata.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&host_path, perms)?;
        }
    }
    Ok(())
}

fn task_boundary_ext_value(task_boundary: &TaskBoundaryMaterialization) -> Option<Value> {
    if task_boundary.workspace_files.is_empty()
        && task_boundary.mount_references.is_empty()
        && task_boundary.limits.is_empty()
    {
        return None;
    }

    Some(json!({
        "schema_version": TASK_BOUNDARY_V1_SCHEMA_VERSION,
        "workspace_files": task_boundary.workspace_files,
        "mount_references": task_boundary.mount_references,
        "limits": task_boundary.limits,
    }))
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
    run_id: &str,
    workload_type: &str,
    trial_id: &str,
    variant: &Variant,
    task_idx: usize,
    repl: usize,
    task_boundary: &TaskBoundaryMaterialization,
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
    let mut runtime = serde_json::Map::new();
    runtime.insert("paths".to_string(), runtime_paths);
    runtime.insert(
        "network".to_string(),
        json!({
            "mode_requested": json_value.pointer("/runtime/network/mode").and_then(|v| v.as_str()).unwrap_or("none"),
            "allowed_hosts": json_value.pointer("/runtime/network/allowed_hosts").cloned().unwrap_or(json!([])),
        }),
    );
    runtime.insert(
        "control_plane".to_string(),
        json!({
            "mode": json_value.pointer("/runtime/harness/control_plane/mode").and_then(|v| v.as_str()).unwrap_or("file"),
            "path": control_path,
        }),
    );
    if task_boundary.limits.max_steps.is_some()
        || task_boundary.limits.max_total_tokens.is_some()
        || task_boundary.limits.max_tool_calls.is_some()
    {
        let mut budgets = serde_json::Map::new();
        if let Some(max_steps) = task_boundary.limits.max_steps {
            budgets.insert("max_steps".to_string(), json!(max_steps));
        }
        if let Some(max_total_tokens) = task_boundary.limits.max_total_tokens {
            budgets.insert("max_total_tokens".to_string(), json!(max_total_tokens));
        }
        if let Some(max_tool_calls) = task_boundary.limits.max_tool_calls {
            budgets.insert("max_tool_calls".to_string(), json!(max_tool_calls));
        }
        runtime.insert("budgets".to_string(), Value::Object(budgets));
    }
    if task_boundary.limits.trial_seconds.is_some() {
        runtime.insert(
            "timeouts".to_string(),
            json!({
                "trial_seconds": task_boundary.limits.trial_seconds,
            }),
        );
    }

    let mut input = json!({
        "schema_version": "trial_input_v1",
        "ids": {
            "run_id": run_id,
            "trial_id": trial_id,
            "variant_id": variant.id,
            "task_id": task_boundary.task_payload.get("id").and_then(|v| v.as_str()).unwrap_or(&format!("task_{}", task_idx)),
            "repl_idx": repl
        },
        "task": task_boundary.task_payload.clone(),
        "workload": {
            "type": workload_type
        },
        "bindings": variant.bindings.clone(),
        "design": {
            "sanitization_profile": json_value.pointer("/design/sanitization_profile").and_then(|v| v.as_str()).unwrap_or("hermetic_functional_v2"),
            "integration_level": json_value.pointer("/runtime/harness/integration_level").and_then(|v| v.as_str()).unwrap_or("cli_basic"),
        },
        "runtime": Value::Object(runtime),
    });
    if let Some(task_boundary_ext) = task_boundary_ext_value(task_boundary) {
        if let Some(obj) = input.as_object_mut() {
            obj.insert(
                "ext".to_string(),
                json!({ "task_boundary_v1": task_boundary_ext }),
            );
        }
    }
    input
}

fn sanitize_for_fs(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "chain".to_string()
    } else {
        out
    }
}

fn append_jsonl(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    writeln!(&mut file)?;
    Ok(())
}

fn collect_workspace_snapshot_manifest(workspace: &Path) -> Result<Value> {
    let mut files: Vec<(String, String, u64)> = Vec::new();
    if workspace.exists() {
        let walker = walkdir::WalkDir::new(workspace).into_iter();
        for entry in walker {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(workspace)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            let digest = sha256_file(entry.path())?;
            let size = entry.metadata()?.len();
            files.push((rel, digest, size));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let total_bytes = files.iter().map(|(_, _, sz)| *sz).sum::<u64>();
    let rows = files
        .into_iter()
        .map(|(path, digest, size_bytes)| {
            json!({
                "path": path,
                "digest": digest,
                "size_bytes": size_bytes
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "schema_version": "workspace_snapshot_v1",
        "captured_at": Utc::now().to_rfc3339(),
        "file_count": rows.len(),
        "total_bytes": total_bytes,
        "files": rows
    }))
}

fn snapshot_file_map(snapshot_manifest: &Value) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(arr) = snapshot_manifest.get("files").and_then(|v| v.as_array()) {
        for row in arr {
            let path = row.get("path").and_then(|v| v.as_str());
            let digest = row.get("digest").and_then(|v| v.as_str());
            if let (Some(path), Some(digest)) = (path, digest) {
                map.insert(path.to_string(), digest.to_string());
            }
        }
    }
    map
}

fn diff_workspace_snapshots(prev: &Value, post: &Value) -> Value {
    let prev_map = snapshot_file_map(prev);
    let post_map = snapshot_file_map(post);

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut modified = Vec::new();

    for (path, digest) in post_map.iter() {
        match prev_map.get(path) {
            None => added.push(path.clone()),
            Some(prev_digest) if prev_digest != digest => modified.push(path.clone()),
            _ => {}
        }
    }
    for path in prev_map.keys() {
        if !post_map.contains_key(path) {
            removed.push(path.clone());
        }
    }

    json!({
        "schema_version": "workspace_diff_v1",
        "captured_at": Utc::now().to_rfc3339(),
        "added": added,
        "removed": removed,
        "modified": modified,
        "summary": {
            "added_files": added.len(),
            "removed_files": removed.len(),
            "modified_files": modified.len()
        }
    })
}

fn derive_patch_from_diff(diff: &Value) -> Value {
    json!({
        "schema_version": "workspace_patch_v1",
        "format": "file_digest_delta",
        "generated_at": Utc::now().to_rfc3339(),
        "added": diff.get("added").cloned().unwrap_or(json!([])),
        "removed": diff.get("removed").cloned().unwrap_or(json!([])),
        "modified": diff.get("modified").cloned().unwrap_or(json!([])),
    })
}

fn restore_workspace_from_snapshot(snapshot_dir: &Path, workspace_dir: &Path) -> Result<()> {
    if workspace_dir.exists() {
        fs::remove_dir_all(workspace_dir)?;
    }
    ensure_dir(workspace_dir)?;
    copy_dir_filtered(snapshot_dir, workspace_dir, &[])?;
    Ok(())
}

fn resolve_chain_label(
    task_payload: &Value,
    task_id: &str,
    state_policy: StatePolicy,
) -> String {
    let explicit = task_payload
        .get("chain_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(label) = explicit {
        return label;
    }
    match state_policy {
        StatePolicy::PersistPerTask => task_id.to_string(),
        StatePolicy::Accumulate => "global".to_string(),
        StatePolicy::IsolatePerTrial => task_id.to_string(),
    }
}

fn rel_to_run_dir(path: &Path, run_dir: &Path) -> String {
    path.strip_prefix(run_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

struct ProcessRunResult {
    status: String,
    stdout: String,
    stderr: String,
}

fn run_harness_local(
    harness: &HarnessConfig,
    paths: &TrialPaths,
    input_path: &Path,
    output_path: &Path,
    control_path: &str,
    command: &[String],
) -> Result<ProcessRunResult> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    cmd.current_dir(&paths.workspace);
    cmd.env("AGENTLAB_TRIAL_INPUT", &input_path);
    cmd.env("AGENTLAB_TRIAL_OUTPUT", &output_path);
    cmd.env("AGENTLAB_CONTROL_PATH", control_path);
    cmd.env("AGENTLAB_HARNESS_ROOT", &paths.exp_dir);
    if harness.tracing_mode.as_deref() == Some("otlp") {
        cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318");
    }
    run_process_with_trial_io(cmd, input_path, output_path)
}

fn run_harness_container(
    json_value: &Value,
    harness: &HarnessConfig,
    paths: &TrialPaths,
    dynamic_mounts: &[ResolvedMountReference],
    input_path: &Path,
    output_path: &Path,
    control_path: &str,
    command: &[String],
    network_mode: &str,
    setup_command: Option<&str>,
) -> Result<ProcessRunResult> {
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
    // Keep harness code/dependencies isolated from mutable task state.
    cmd.args(["-v", &format!("{}:/harness:ro", paths.exp_dir.display())]);
    cmd.args(["-v", &format!("{}:/state", paths.state.display())]);
    cmd.args(["-v", &format!("{}:/dataset:ro", paths.dataset.display())]);
    cmd.args(["-v", &format!("{}:/out", paths.out.display())]);
    for mount in dynamic_mounts {
        cmd.args([
            "-v",
            &format!("{}:{}:ro", mount.host_path.display(), mount.mount_path),
        ]);
    }
    cmd.args(["--tmpfs", "/tmp:rw"]);
    cmd.args(["-w", "/workspace"]);

    cmd.arg("-e")
        .arg(format!("AGENTLAB_TRIAL_INPUT={}", harness.input_path));
    cmd.arg("-e")
        .arg(format!("AGENTLAB_TRIAL_OUTPUT={}", harness.output_path));
    cmd.arg("-e")
        .arg(format!("AGENTLAB_CONTROL_PATH={}", control_path));
    cmd.arg("-e").arg("AGENTLAB_HARNESS_ROOT=/harness");

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
            resolved.push(format!("/harness/{}", rel));
        } else if p.is_absolute() && p.starts_with(exp_dir) {
            if let Ok(rel) = p.strip_prefix(exp_dir) {
                let rel = rel.to_string_lossy().trim_start_matches('/').to_string();
                resolved.push(format!("/harness/{}", rel));
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
) -> Result<ProcessRunResult> {
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

    Ok(ProcessRunResult {
        status: output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
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
        || events_path.starts_with("/harness")
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
            json!({"name": "harness", "path": "/harness", "writable": false}),
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

fn remove_path_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn apply_materialization_policy(trial_dir: &Path, mode: MaterializationMode) -> Result<()> {
    match mode {
        MaterializationMode::Full => return Ok(()),
        MaterializationMode::OutputsOnly => {
            for dir_name in ["workspace", "dataset", "state", "tmp", "artifacts"] {
                remove_path_if_exists(&trial_dir.join(dir_name))?;
            }
        }
        MaterializationMode::MetadataOnly | MaterializationMode::None => {
            for dir_name in ["workspace", "dataset", "state", "tmp", "artifacts", "out"] {
                remove_path_if_exists(&trial_dir.join(dir_name))?;
            }
            remove_path_if_exists(&trial_dir.join("trial_input.json"))?;
            remove_path_if_exists(&trial_dir.join("trial_output.json"))?;
            remove_path_if_exists(&trial_dir.join("harness_manifest.json"))?;
            remove_path_if_exists(&trial_dir.join("trace_manifest.json"))?;
            if matches!(mode, MaterializationMode::None) {
                remove_path_if_exists(&trial_dir.join("state_inventory.json"))?;
            }
        }
    }
    Ok(())
}

fn map_container_path_to_host(path: &str, paths: &TrialPaths) -> PathBuf {
    if let Some(rest) = path.strip_prefix("/state") {
        paths.state.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = path.strip_prefix("/out") {
        paths.out.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = path.strip_prefix("/harness") {
        paths.exp_dir.join(rest.trim_start_matches('/'))
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

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "{}_{}_{}",
                prefix,
                std::process::id(),
                Utc::now().timestamp_micros()
            ));
            ensure_dir(&path).expect("temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn create_run_dir(prefix: &str, run_id: &str) -> (TempDirGuard, PathBuf) {
        let root = TempDirGuard::new(prefix);
        let run_dir = root.path.join(".lab").join("runs").join(run_id);
        ensure_dir(&run_dir).expect("run dir");
        (root, run_dir)
    }

    fn harness_success_command() -> Vec<String> {
        vec![
            "sh".to_string(),
            "-lc".to_string(),
            "printf '%s' '{\"schema_version\":\"trial_output_v1\",\"outcome\":\"success\",\"checkpoints\":[]}' > \"$AGENTLAB_TRIAL_OUTPUT\"".to_string(),
        ]
    }

    fn write_resolved_experiment(
        run_dir: &Path,
        integration_level: &str,
        include_events_path: bool,
    ) {
        let mut harness = serde_json::Map::new();
        harness.insert(
            "command".to_string(),
            Value::Array(
                harness_success_command()
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
        harness.insert(
            "integration_level".to_string(),
            Value::String(integration_level.to_string()),
        );
        harness.insert(
            "input_path".to_string(),
            Value::String("/out/trial_input.json".to_string()),
        );
        harness.insert(
            "output_path".to_string(),
            Value::String("/out/trial_output.json".to_string()),
        );
        harness.insert(
            "control_plane".to_string(),
            json!({
                "path": "/state/lab_control.json"
            }),
        );
        if include_events_path {
            harness.insert(
                "events".to_string(),
                json!({
                    "path": "/state/harness_events.jsonl"
                }),
            );
        }

        let resolved = json!({
            "runtime": {
                "harness": Value::Object(harness),
                "network": { "mode": "none" }
            }
        });
        atomic_write_json_pretty(&run_dir.join("resolved_experiment.json"), &resolved)
            .expect("write resolved");
    }

    fn seed_parent_trial(
        run_dir: &Path,
        trial_id: &str,
        checkpoints: Value,
        trial_status: &str,
        pause_label: Option<&str>,
    ) -> PathBuf {
        let trial_dir = run_dir.join("trials").join(trial_id);
        ensure_dir(&trial_dir).expect("trial dir");
        ensure_dir(&trial_dir.join("workspace")).expect("workspace");
        ensure_dir(&trial_dir.join("state")).expect("state");
        ensure_dir(&trial_dir.join("dataset")).expect("dataset");

        fs::write(
            trial_dir.join("workspace").join("fixture.txt"),
            "workspace fixture",
        )
        .expect("workspace fixture");
        fs::write(
            trial_dir.join("dataset").join("tasks.jsonl"),
            "{\"id\":\"task_1\"}\n",
        )
        .expect("dataset file");

        let trial_input = json!({
            "schema_version": "trial_input_v1",
            "ids": { "trial_id": trial_id },
            "bindings": {
                "existing": "value"
            },
            "runtime": {
                "paths": {
                    "workspace": trial_dir.join("workspace").to_string_lossy().to_string(),
                    "state": trial_dir.join("state").to_string_lossy().to_string(),
                    "dataset": trial_dir.join("dataset").to_string_lossy().to_string(),
                    "out": trial_dir.join("out").to_string_lossy().to_string(),
                    "tmp": trial_dir.join("tmp").to_string_lossy().to_string()
                },
                "network": { "mode_requested": "none" }
            }
        });
        atomic_write_json_pretty(&trial_dir.join("trial_input.json"), &trial_input)
            .expect("trial input");

        let trial_output = json!({
            "schema_version": "trial_output_v1",
            "outcome": "success",
            "checkpoints": checkpoints
        });
        atomic_write_json_pretty(&trial_dir.join("trial_output.json"), &trial_output)
            .expect("trial output");

        write_trial_state(
            &trial_dir,
            trial_id,
            trial_status,
            pause_label,
            pause_label,
            if trial_status == "paused" {
                Some("paused_by_user")
            } else {
                None
            },
        )
        .expect("trial state");

        trial_dir
    }

    fn spawn_pause_ack_writer(
        control_path: PathBuf,
        events_path: PathBuf,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen_versions = std::collections::BTreeSet::new();
            while Instant::now() < deadline {
                let bytes = match fs::read(&control_path) {
                    Ok(b) => b,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                };
                let value: Value = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                };
                let action = value
                    .pointer("/action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("continue");
                if action != "checkpoint" && action != "stop" {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }

                let version = sha256_bytes(&bytes);
                if !seen_versions.insert(version.clone()) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }

                if let Some(parent) = events_path.parent() {
                    let _ = ensure_dir(parent);
                }
                let ack = json!({
                    "event_type": "control_ack",
                    "action_observed": action,
                    "control_version": version
                });
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&events_path)
                {
                    let _ = writeln!(file, "{}", ack);
                }
                if action == "stop" {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        })
    }

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
    fn resolve_resume_selector_errors_when_label_not_found() {
        let root = TempDirGuard::new("agentlab_resume_missing_label_test");
        let trial_dir = root.path.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let output = json!({
            "schema_version": "trial_output_v1",
            "outcome": "success",
            "checkpoints": [
                {"path": "/state/ckpt_a", "logical_name": "a", "step": 1}
            ]
        });
        atomic_write_json_pretty(&trial_dir.join("trial_output.json"), &output).expect("write");
        let err = resolve_resume_selector(&trial_dir, Some("missing")).expect_err("should fail");
        assert!(
            err.to_string().contains("resume_checkpoint_not_found"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn parse_fork_selector_rejects_empty_checkpoint_name() {
        let err = match parse_fork_selector("checkpoint: ") {
            Ok(_) => panic!("empty checkpoint should fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("checkpoint name empty"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn resolve_selector_checkpoint_non_strict_returns_none_when_path_missing() {
        let root = TempDirGuard::new("agentlab_fork_selector_path_missing");
        let trial_dir = root.path.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let output = json!({
            "checkpoints": [
                {"path": "/state/cp_missing", "logical_name": "cp1", "step": 3}
            ]
        });
        let selector = parse_fork_selector("checkpoint:cp1").expect("selector");
        let source = resolve_selector_checkpoint(&selector, Some(&output), &trial_dir, false)
            .expect("selector resolution");
        assert_eq!(source, None);
    }

    #[test]
    fn resolve_selector_checkpoint_strict_requires_existing_checkpoint_path() {
        let root = TempDirGuard::new("agentlab_fork_selector_strict_missing");
        let trial_dir = root.path.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let output = json!({
            "checkpoints": [
                {"path": "/state/cp_missing", "logical_name": "cp1", "step": 3}
            ]
        });
        let selector = parse_fork_selector("checkpoint:cp1").expect("selector");
        let err = resolve_selector_checkpoint(&selector, Some(&output), &trial_dir, true)
            .expect_err("strict resolution should fail");
        assert!(
            err.to_string().contains("strict_source_unavailable"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn fork_trial_non_strict_falls_back_to_input_only_when_checkpoint_missing() {
        let (_root, run_dir) = create_run_dir("agentlab_fork_input_fallback", "run_1");
        write_resolved_experiment(&run_dir, "cli_events", true);
        seed_parent_trial(
            &run_dir,
            "trial_1",
            json!([{"path": "/state/cp_missing", "logical_name": "cp1", "step": 1}]),
            "completed",
            None,
        );

        let result = fork_trial(
            &run_dir,
            "trial_1",
            "checkpoint:cp1",
            &BTreeMap::new(),
            false,
        )
        .expect("fork should succeed");
        assert_eq!(result.fallback_mode, "input_only");
        assert_eq!(result.source_checkpoint, None);

        let manifest = load_json_file(&result.fork_dir.join("manifest.json")).expect("manifest");
        assert_eq!(
            manifest
                .pointer("/fallback_mode")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "input_only"
        );
        assert!(manifest.pointer("/source_checkpoint").is_some());
    }

    #[test]
    fn fork_trial_strict_requires_sdk_full_integration_level() {
        let (_root, run_dir) = create_run_dir("agentlab_fork_strict_level", "run_1");
        write_resolved_experiment(&run_dir, "cli_events", true);
        seed_parent_trial(
            &run_dir,
            "trial_1",
            json!([{"path": "/state/cp1", "logical_name": "cp1", "step": 1}]),
            "completed",
            None,
        );

        let err = fork_trial(
            &run_dir,
            "trial_1",
            "checkpoint:cp1",
            &BTreeMap::new(),
            true,
        )
        .err()
        .expect("strict fork should fail for non-sdk_full");
        assert!(
            err.to_string()
                .contains("strict fork requires integration_level sdk_full"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn fork_trial_strict_fails_when_selected_checkpoint_is_unavailable() {
        let (_root, run_dir) = create_run_dir("agentlab_fork_strict_checkpoint", "run_1");
        write_resolved_experiment(&run_dir, "sdk_full", true);
        seed_parent_trial(
            &run_dir,
            "trial_1",
            json!([{"path": "/state/cp_missing", "logical_name": "cp1", "step": 1}]),
            "completed",
            None,
        );

        let err = fork_trial(
            &run_dir,
            "trial_1",
            "checkpoint:cp1",
            &BTreeMap::new(),
            true,
        )
        .err()
        .expect("strict fork should fail when checkpoint bytes are unavailable");
        assert!(
            err.to_string().contains("strict_source_unavailable"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn pause_run_rejects_target_trial_that_is_not_active() {
        let (_root, run_dir) = create_run_dir("agentlab_pause_not_active", "run_1");
        write_resolved_experiment(&run_dir, "cli_events", true);
        let trial_dir = seed_parent_trial(&run_dir, "trial_1", json!([]), "running", None);
        let control_path = trial_dir.join("state").join("lab_control.json");
        write_control_file(&control_path).expect("control file");
        write_run_control(
            &run_dir,
            "run_1",
            "running",
            Some("trial_1"),
            Some(&control_path),
        )
        .expect("run control");

        let err = pause_run(&run_dir, Some("trial_2"), Some("pause"), 1)
            .err()
            .expect("pause should reject non-active target");
        assert!(
            err.to_string().contains("pause_target_not_active"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn pause_run_requires_events_path_for_supported_integration_levels() {
        let (_root, run_dir) = create_run_dir("agentlab_pause_events_required", "run_1");
        write_resolved_experiment(&run_dir, "cli_events", false);
        let trial_dir = seed_parent_trial(&run_dir, "trial_1", json!([]), "running", None);
        let control_path = trial_dir.join("state").join("lab_control.json");
        write_control_file(&control_path).expect("control file");
        write_run_control(
            &run_dir,
            "run_1",
            "running",
            Some("trial_1"),
            Some(&control_path),
        )
        .expect("run control");

        let err = pause_run(&run_dir, None, Some("pause"), 1)
            .err()
            .expect("pause should fail when events path is missing");
        assert!(
            err.to_string().contains("pause_requires_events_path"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn pause_run_completes_checkpoint_then_stop_and_updates_state() {
        let (_root, run_dir) = create_run_dir("agentlab_pause_success", "run_1");
        write_resolved_experiment(&run_dir, "cli_events", true);
        let trial_dir = seed_parent_trial(&run_dir, "trial_1", json!([]), "running", None);
        let control_path = trial_dir.join("state").join("lab_control.json");
        let events_path = trial_dir.join("state").join("harness_events.jsonl");
        write_control_file(&control_path).expect("control file");
        write_run_control(
            &run_dir,
            "run_1",
            "running",
            Some("trial_1"),
            Some(&control_path),
        )
        .expect("run control");

        let ack_thread = spawn_pause_ack_writer(control_path.clone(), events_path);
        let paused = pause_run(&run_dir, None, Some("manual_pause"), 2).expect("pause success");
        ack_thread.join().expect("ack writer thread");

        assert_eq!(paused.run_id, "run_1");
        assert_eq!(paused.trial_id, "trial_1");
        assert_eq!(paused.label, "manual_pause");
        assert!(paused.checkpoint_acked);
        assert!(paused.stop_acked);

        let run_control = load_json_file(&run_control_path(&run_dir)).expect("run control");
        assert_eq!(
            run_control
                .pointer("/status")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "paused"
        );
        assert_eq!(
            run_control
                .pointer("/active_trial_id")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "trial_1"
        );

        let trial_state = load_json_file(&trial_dir.join("trial_state.json")).expect("trial state");
        assert_eq!(
            trial_state
                .pointer("/status")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "paused"
        );
        assert_eq!(
            trial_state
                .pointer("/pause_label")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "manual_pause"
        );
        assert_eq!(
            trial_state
                .pointer("/checkpoint_selected")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "manual_pause"
        );
        assert_eq!(
            trial_state
                .pointer("/exit_reason")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "paused_by_user"
        );
    }

    #[test]
    fn resume_run_requires_run_to_be_paused() {
        let (_root, run_dir) = create_run_dir("agentlab_resume_not_paused", "run_1");
        write_resolved_experiment(&run_dir, "sdk_full", true);
        let trial_dir = seed_parent_trial(
            &run_dir,
            "trial_1",
            json!([{"path": "/state/cp1", "logical_name": "cp1", "step": 1}]),
            "paused",
            Some("cp1"),
        );
        ensure_dir(&trial_dir.join("state").join("cp1")).expect("checkpoint path");
        write_run_control(&run_dir, "run_1", "running", Some("trial_1"), None)
            .expect("run control");

        let err = resume_run(&run_dir, None, None, &BTreeMap::new(), false)
            .err()
            .expect("resume should fail for non-paused run");
        assert!(
            err.to_string().contains("resume_non_paused"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn resume_run_requires_trial_state_to_be_paused() {
        let (_root, run_dir) = create_run_dir("agentlab_resume_trial_state", "run_1");
        write_resolved_experiment(&run_dir, "sdk_full", true);
        let trial_dir = seed_parent_trial(
            &run_dir,
            "trial_1",
            json!([{"path": "/state/cp1", "logical_name": "cp1", "step": 1}]),
            "completed",
            None,
        );
        ensure_dir(&trial_dir.join("state").join("cp1")).expect("checkpoint path");
        write_run_control(&run_dir, "run_1", "paused", Some("trial_1"), None).expect("run control");

        let err = resume_run(&run_dir, None, None, &BTreeMap::new(), false)
            .err()
            .expect("resume should fail when trial state is not paused");
        assert!(
            err.to_string().contains("resume_trial_not_paused"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn resume_run_uses_pause_label_and_forks_with_binding_overrides() {
        let (_root, run_dir) = create_run_dir("agentlab_resume_success", "run_1");
        write_resolved_experiment(&run_dir, "sdk_full", true);
        let trial_dir = seed_parent_trial(
            &run_dir,
            "trial_1",
            json!([
                {"path": "/state/cp_old", "logical_name": "cp_old", "step": 1},
                {"path": "/state/cp_resume", "logical_name": "cp_resume", "step": 2}
            ]),
            "paused",
            Some("cp_resume"),
        );
        ensure_dir(&trial_dir.join("state").join("cp_resume")).expect("checkpoint path");
        write_run_control(&run_dir, "run_1", "paused", Some("trial_1"), None).expect("run control");

        let mut set_bindings = BTreeMap::new();
        set_bindings.insert("resume.override".to_string(), json!(42));
        let resumed =
            resume_run(&run_dir, None, None, &set_bindings, false).expect("resume success");

        assert_eq!(resumed.trial_id, "trial_1");
        assert_eq!(resumed.selector, "checkpoint:cp_resume");
        assert_eq!(resumed.fork.parent_trial_id, "trial_1");
        assert_eq!(resumed.fork.fallback_mode, "checkpoint");
        assert!(resumed.fork.source_checkpoint.is_some());

        let fork_input = load_json_file(
            &resumed
                .fork
                .fork_dir
                .join("trial_1")
                .join("trial_input.json"),
        )
        .expect("fork trial input");
        assert_eq!(
            fork_input
                .pointer("/bindings/resume/override")
                .and_then(|v| v.as_i64())
                .unwrap_or_default(),
            42
        );
        assert_eq!(
            fork_input
                .pointer("/ext/fork/selector")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "checkpoint:cp_resume"
        );
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

    #[test]
    fn parse_task_boundary_extracts_runtime_fields() {
        let task = json!({
            "schema_version": "task_boundary_v1",
            "task": {
                "id": "task_1",
                "prompt": "solve this"
            },
            "workspace_files": [
                { "path": "notes/input.txt", "content": "hello" }
            ],
            "mount_references": [
                {
                    "dataset_pack_ref": format!("sha256:{}", "a".repeat(64)),
                    "mount_path": "/workspace/dataset_pack",
                    "read_only": true
                }
            ],
            "limits": {
                "max_steps": 8,
                "max_total_tokens": 2048,
                "max_tool_calls": 4,
                "trial_seconds": 120
            }
        });

        let parsed = parse_task_boundary_from_dataset_task(&task).expect("parse boundary");
        assert_eq!(
            parsed
                .task_payload
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "task_1"
        );
        assert_eq!(parsed.workspace_files.len(), 1);
        assert_eq!(parsed.mount_references.len(), 1);
        assert_eq!(parsed.limits.max_steps, Some(8));
        assert_eq!(parsed.limits.max_total_tokens, Some(2048));
        assert_eq!(parsed.limits.max_tool_calls, Some(4));
        assert_eq!(parsed.limits.trial_seconds, Some(120));
    }

    #[test]
    fn parse_task_boundary_rejects_unsupported_keys() {
        let task = json!({
            "schema_version": "task_boundary_v1",
            "task": { "id": "task_1" },
            "workspace_files": [],
            "mount_references": [],
            "limits": {},
            "benchmark_kind": "custom_magic"
        });
        let err = parse_task_boundary_from_dataset_task(&task).expect_err("should fail");
        assert!(
            err.to_string().contains("unsupported key"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn parse_task_boundary_from_trial_input_legacy_without_task_defaults_empty() {
        let input = json!({
            "schema_version": "trial_input_v1",
            "ids": { "trial_id": "trial_1" },
            "runtime": {
                "paths": {
                    "workspace": "/tmp/workspace"
                }
            }
        });

        let parsed = parse_task_boundary_from_trial_input(&input).expect("parse legacy input");
        assert_eq!(
            parsed
                .task_payload
                .as_object()
                .map(|obj| obj.len())
                .unwrap_or_default(),
            0
        );
        assert!(parsed.workspace_files.is_empty());
        assert!(parsed.mount_references.is_empty());
        assert!(parsed.limits.is_empty());
    }

    #[test]
    fn materialize_workspace_files_writes_utf8_and_base64() {
        let root = TempDirGuard::new("agentlab_task_boundary_workspace_files");
        let exp_dir = root.path.join("exp");
        ensure_dir(&exp_dir).expect("exp dir");
        fs::write(exp_dir.join("README.md"), "fixture").expect("exp fixture");
        let dataset_src = root.path.join("tasks.jsonl");
        fs::write(&dataset_src, "{\"id\":\"task_1\"}\n").expect("dataset");
        let trial_dir = root.path.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let paths = TrialPaths::new(&trial_dir, &exp_dir, &dataset_src).expect("trial paths");
        paths.prepare().expect("prepare");

        let files = vec![
            WorkspaceFileSpec {
                path: "notes/plain.txt".to_string(),
                content: "hello world".to_string(),
                encoding: Some("utf8".to_string()),
                executable: false,
            },
            WorkspaceFileSpec {
                path: "notes/decoded.txt".to_string(),
                content: "aGVsbG8gYmFzZTY0".to_string(),
                encoding: Some("base64".to_string()),
                executable: false,
            },
        ];

        materialize_workspace_files(&paths, &files).expect("materialize");
        assert_eq!(
            fs::read_to_string(paths.workspace.join("notes/plain.txt")).expect("plain"),
            "hello world"
        );
        assert_eq!(
            fs::read_to_string(paths.workspace.join("notes/decoded.txt")).expect("decoded"),
            "hello base64"
        );
    }

    #[test]
    fn resolve_task_mounts_requires_container_and_existing_pack() {
        let root = TempDirGuard::new("agentlab_task_boundary_mounts");
        let digest = "b".repeat(64);
        let pack_dir = root.path.join(".lab").join("dataset_packs").join("sha256");
        ensure_dir(&pack_dir).expect("pack dir");
        fs::write(pack_dir.join(&digest), "pack bytes").expect("pack file");

        let refs = vec![MountReferenceSpec {
            dataset_pack_ref: format!("sha256:{}", digest),
            mount_path: "/workspace/dataset_pack".to_string(),
            read_only: true,
        }];
        let resolved = resolve_task_mounts(&root.path, &refs, true).expect("resolve mounts");
        assert_eq!(resolved.len(), 1);
        assert!(
            resolved[0].host_path.ends_with(Path::new(&digest)),
            "unexpected host path: {}",
            resolved[0].host_path.display()
        );

        let err =
            resolve_task_mounts(&root.path, &refs, false).expect_err("local mode should fail");
        assert!(
            err.to_string().contains("require container"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn build_trial_input_uses_run_id_and_limits() {
        let root = TempDirGuard::new("agentlab_task_boundary_trial_input");
        let exp_dir = root.path.join("exp");
        ensure_dir(&exp_dir).expect("exp");
        fs::write(exp_dir.join("harness.sh"), "#!/bin/sh\n").expect("harness");
        let dataset_src = root.path.join("tasks.jsonl");
        fs::write(&dataset_src, "{\"id\":\"task_1\"}\n").expect("dataset");
        let trial_dir = root.path.join("trial_1");
        ensure_dir(&trial_dir).expect("trial");
        let paths = TrialPaths::new(&trial_dir, &exp_dir, &dataset_src).expect("paths");
        paths.prepare().expect("prepare");

        let json_value = json!({
            "design": { "sanitization_profile": "hermetic_functional_v2" },
            "runtime": {
                "harness": {
                    "integration_level": "cli_events",
                    "control_plane": { "mode": "file", "path": "/state/lab_control.json" }
                },
                "network": { "mode": "none", "allowed_hosts": [] }
            }
        });
        let variant = Variant {
            id: "baseline".to_string(),
            bindings: json!({ "model": "demo" }),
        };
        let task_boundary = TaskBoundaryMaterialization {
            task_payload: json!({ "id": "task_1", "prompt": "x" }),
            workspace_files: vec![WorkspaceFileSpec {
                path: "input.txt".to_string(),
                content: "hello".to_string(),
                encoding: Some("utf8".to_string()),
                executable: false,
            }],
            mount_references: vec![MountReferenceSpec {
                dataset_pack_ref: format!("sha256:{}", "c".repeat(64)),
                mount_path: "/workspace/dataset_pack".to_string(),
                read_only: true,
            }],
            limits: TaskBoundaryLimits {
                max_steps: Some(12),
                max_total_tokens: Some(4096),
                max_tool_calls: Some(9),
                trial_seconds: Some(90),
            },
        };

        let input = build_trial_input(
            &json_value,
            "run_actual_1",
            "agent_harness",
            "trial_1",
            &variant,
            0,
            0,
            &task_boundary,
            &paths,
            true,
        );

        assert_eq!(
            input
                .pointer("/ids/run_id")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "run_actual_1"
        );
        assert_eq!(
            input
                .pointer("/runtime/budgets/max_steps")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            12
        );
        assert_eq!(
            input
                .pointer("/runtime/timeouts/trial_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            90
        );
        assert_eq!(
            input
                .pointer("/ext/task_boundary_v1/workspace_files/0/path")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "input.txt"
        );
    }

    // -----------------------------------------------------------------------
    // build_trial_schedule tests
    // -----------------------------------------------------------------------

    #[test]
    fn schedule_variant_sequential_orders_variant_then_task_then_repl() {
        let slots = build_trial_schedule(2, 3, 2, SchedulingPolicy::VariantSequential, 1);
        assert_eq!(slots.len(), 12); // 2 variants * 3 tasks * 2 repls

        // First 6 slots should be variant 0
        for slot in &slots[0..6] {
            assert_eq!(slot.variant_idx, 0);
        }
        // Last 6 slots should be variant 1
        for slot in &slots[6..12] {
            assert_eq!(slot.variant_idx, 1);
        }

        // Within variant 0: task 0 repl 0, task 0 repl 1, task 1 repl 0, ...
        assert_eq!(slots[0].task_idx, 0);
        assert_eq!(slots[0].repl_idx, 0);
        assert_eq!(slots[1].task_idx, 0);
        assert_eq!(slots[1].repl_idx, 1);
        assert_eq!(slots[2].task_idx, 1);
        assert_eq!(slots[2].repl_idx, 0);
    }

    #[test]
    fn schedule_paired_interleaved_orders_task_then_variant_then_repl() {
        let slots = build_trial_schedule(2, 3, 2, SchedulingPolicy::PairedInterleaved, 1);
        assert_eq!(slots.len(), 12);

        // First 4 slots should all be task 0 (2 variants * 2 repls)
        for slot in &slots[0..4] {
            assert_eq!(slot.task_idx, 0);
        }
        // Within task 0: variant 0 repl 0, variant 0 repl 1, variant 1 repl 0, variant 1 repl 1
        assert_eq!(slots[0].variant_idx, 0);
        assert_eq!(slots[0].repl_idx, 0);
        assert_eq!(slots[1].variant_idx, 0);
        assert_eq!(slots[1].repl_idx, 1);
        assert_eq!(slots[2].variant_idx, 1);
        assert_eq!(slots[2].repl_idx, 0);
        assert_eq!(slots[3].variant_idx, 1);
        assert_eq!(slots[3].repl_idx, 1);
    }

    #[test]
    fn schedule_paired_interleaved_pairs_variants_on_same_task() {
        // Key A/B test property: for each task, all variants run before moving to next task
        let slots = build_trial_schedule(3, 4, 1, SchedulingPolicy::PairedInterleaved, 1);
        assert_eq!(slots.len(), 12); // 3 variants * 4 tasks * 1 repl

        for task_idx in 0..4 {
            let task_slots: Vec<_> = slots.iter().filter(|s| s.task_idx == task_idx).collect();
            assert_eq!(task_slots.len(), 3); // one per variant
            let variant_ids: Vec<_> = task_slots.iter().map(|s| s.variant_idx).collect();
            assert_eq!(variant_ids, vec![0, 1, 2]);
        }
    }

    #[test]
    fn schedule_randomized_contains_all_slots() {
        let slots = build_trial_schedule(2, 3, 2, SchedulingPolicy::Randomized, 42);
        assert_eq!(slots.len(), 12);

        // Every (variant, task, repl) triple should appear exactly once
        let mut seen = HashSet::new();
        for slot in &slots {
            let key = (slot.variant_idx, slot.task_idx, slot.repl_idx);
            assert!(seen.insert(key), "duplicate slot: {:?}", key);
        }
        assert_eq!(seen.len(), 12);
    }

    #[test]
    fn schedule_randomized_is_deterministic_with_same_seed() {
        let a = build_trial_schedule(2, 4, 2, SchedulingPolicy::Randomized, 1337);
        let b = build_trial_schedule(2, 4, 2, SchedulingPolicy::Randomized, 1337);
        for (sa, sb) in a.iter().zip(b.iter()) {
            assert_eq!(sa.variant_idx, sb.variant_idx);
            assert_eq!(sa.task_idx, sb.task_idx);
            assert_eq!(sa.repl_idx, sb.repl_idx);
        }
    }

    #[test]
    fn schedule_randomized_different_seed_produces_different_order() {
        let a = build_trial_schedule(2, 4, 2, SchedulingPolicy::Randomized, 1);
        let b = build_trial_schedule(2, 4, 2, SchedulingPolicy::Randomized, 2);
        // With 16 slots, the probability of identical ordering is negligible
        let same = a.iter().zip(b.iter()).all(|(sa, sb)| {
            sa.variant_idx == sb.variant_idx
                && sa.task_idx == sb.task_idx
                && sa.repl_idx == sb.repl_idx
        });
        assert!(!same, "different seeds should produce different orderings");
    }

    #[test]
    fn schedule_single_variant_single_task_single_repl() {
        for policy in [
            SchedulingPolicy::VariantSequential,
            SchedulingPolicy::PairedInterleaved,
            SchedulingPolicy::Randomized,
        ] {
            let slots = build_trial_schedule(1, 1, 1, policy, 1);
            assert_eq!(slots.len(), 1);
            assert_eq!(slots[0].variant_idx, 0);
            assert_eq!(slots[0].task_idx, 0);
            assert_eq!(slots[0].repl_idx, 0);
        }
    }

    #[test]
    fn schedule_empty_when_zero_tasks() {
        let slots = build_trial_schedule(2, 0, 3, SchedulingPolicy::VariantSequential, 1);
        assert!(slots.is_empty());
    }

    // -----------------------------------------------------------------------
    // should_retry_outcome tests
    // -----------------------------------------------------------------------

    #[test]
    fn retry_with_empty_retry_on_retries_any_failure() {
        // Empty retry_on means retry on any non-success
        assert!(should_retry_outcome("error", "0", &[]));
        assert!(should_retry_outcome("success", "1", &[])); // exit nonzero
        assert!(!should_retry_outcome("success", "0", &[])); // success — no retry
    }

    #[test]
    fn retry_on_error_only_retries_error_outcome() {
        let triggers = vec!["error".to_string()];
        assert!(should_retry_outcome("error", "0", &triggers));
        assert!(should_retry_outcome("error", "1", &triggers));
        assert!(!should_retry_outcome("success", "0", &triggers));
        assert!(!should_retry_outcome("success", "1", &triggers)); // exit nonzero but not "error"
    }

    #[test]
    fn retry_on_failure_retries_nonzero_exit() {
        let triggers = vec!["failure".to_string()];
        assert!(should_retry_outcome("success", "1", &triggers));
        assert!(should_retry_outcome("error", "137", &triggers));
        assert!(!should_retry_outcome("success", "0", &triggers));
        assert!(!should_retry_outcome("error", "0", &triggers)); // error outcome but exit 0
    }

    #[test]
    fn retry_on_timeout_retries_timeout_outcome() {
        let triggers = vec!["timeout".to_string()];
        assert!(should_retry_outcome("timeout", "0", &triggers));
        assert!(should_retry_outcome("timeout", "1", &triggers));
        assert!(!should_retry_outcome("error", "0", &triggers));
        assert!(!should_retry_outcome("success", "0", &triggers));
    }

    #[test]
    fn retry_on_multiple_triggers() {
        let triggers = vec!["error".to_string(), "timeout".to_string()];
        assert!(should_retry_outcome("error", "0", &triggers));
        assert!(should_retry_outcome("timeout", "0", &triggers));
        assert!(!should_retry_outcome("success", "1", &triggers)); // failure not in triggers
    }

    // -----------------------------------------------------------------------
    // parse_policies tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_policies_defaults_when_no_policies_section() {
        let spec = json!({
            "design": {
                "replications": 1,
                "random_seed": 1
            }
        });
        let config = parse_policies(&spec);
        assert_eq!(config.scheduling, SchedulingPolicy::VariantSequential);
        assert_eq!(config.state, StatePolicy::IsolatePerTrial);
        assert_eq!(config.retry_max_attempts, 1);
        assert!(config.retry_on.is_empty());
        assert!(config.pruning_max_consecutive_failures.is_none());
    }

    #[test]
    fn parse_policies_reads_all_fields() {
        let spec = json!({
            "design": {
                "policies": {
                    "scheduling": "paired_interleaved",
                    "state": "persist_per_task",
                    "retry": {
                        "max_attempts": 3,
                        "retry_on": ["error", "timeout"]
                    },
                    "pruning": {
                        "max_consecutive_failures": 5
                    }
                }
            }
        });
        let config = parse_policies(&spec);
        assert_eq!(config.scheduling, SchedulingPolicy::PairedInterleaved);
        assert_eq!(config.state, StatePolicy::PersistPerTask);
        assert_eq!(config.retry_max_attempts, 3);
        assert_eq!(config.retry_on, vec!["error", "timeout"]);
        assert_eq!(config.pruning_max_consecutive_failures, Some(5));
    }

    #[test]
    fn parse_policies_handles_randomized_scheduling() {
        let spec = json!({
            "design": {
                "policies": {
                    "scheduling": "randomized",
                    "state": "accumulate",
                    "retry": { "max_attempts": 1 }
                }
            }
        });
        let config = parse_policies(&spec);
        assert_eq!(config.scheduling, SchedulingPolicy::Randomized);
        assert_eq!(config.state, StatePolicy::Accumulate);
    }

    #[test]
    fn parse_policies_unknown_scheduling_defaults_to_variant_sequential() {
        let spec = json!({
            "design": {
                "policies": {
                    "scheduling": "unknown_value",
                    "state": "unknown_state",
                    "retry": { "max_attempts": 1 }
                }
            }
        });
        let config = parse_policies(&spec);
        assert_eq!(config.scheduling, SchedulingPolicy::VariantSequential);
        assert_eq!(config.state, StatePolicy::IsolatePerTrial);
    }

    #[test]
    fn parse_policies_missing_retry_defaults_to_one_attempt() {
        let spec = json!({
            "design": {
                "policies": {
                    "scheduling": "variant_sequential",
                    "state": "isolate_per_trial"
                }
            }
        });
        let config = parse_policies(&spec);
        assert_eq!(config.retry_max_attempts, 1);
        assert!(config.retry_on.is_empty());
    }
}
