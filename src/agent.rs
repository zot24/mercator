//! Swarm-backed agent runner: REST handlers, job state, and the bridge to
//! the local `swarm` orchestrator. The whole module is gated on the
//! `swarm` feature in main.rs, so the binary builds and ships without any
//! of this code by default.

use crate::AppState;
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentJob {
    pub id: String,
    pub project_name: String,
    pub project_path: String,
    pub prompt: String,
    pub model: String,
    pub permission_mode: String,
    pub max_budget_usd: f64,
    pub status: String, // "running", "succeeded", "failed", "cancelled"
    pub started_at: String,
    pub finished_at: Option<String>,
    pub cost_usd: f64,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub branch: Option<String>,
    pub changed_files: Vec<String>,
}

#[derive(Deserialize)]
pub struct AgentRunRequest {
    project_path: String,
    prompt: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_permission_mode")]
    permission_mode: String,
    #[serde(default = "default_budget")]
    max_budget_usd: f64,
}

fn default_model() -> String {
    "sonnet".to_string()
}
fn default_permission_mode() -> String {
    "acceptEdits".to_string()
}
fn default_budget() -> f64 {
    1.0
}

pub async fn agent_run(
    State(state): State<AppState>,
    Json(req): Json<AgentRunRequest>,
) -> Json<serde_json::Value> {
    let job_id = uuid::Uuid::new_v4().to_string();
    let project_name = Path::new(&req.project_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| req.project_path.clone());

    let job = AgentJob {
        id: job_id.clone(),
        project_name,
        project_path: req.project_path.clone(),
        prompt: req.prompt.clone(),
        model: req.model.clone(),
        permission_mode: req.permission_mode.clone(),
        max_budget_usd: req.max_budget_usd,
        status: "running".to_string(),
        started_at: chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        finished_at: None,
        cost_usd: 0.0,
        tool_calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        summary: None,
        error: None,
        branch: None,
        changed_files: vec![],
    };

    {
        let mut jobs = state.jobs.lock().await;
        jobs.push(job);
    }

    // Spawn the swarm orchestrator in background
    let jobs = state.jobs.clone();
    let task_handles = state.task_handles.clone();
    let job_id_clone = job_id.clone();
    let project_path = req.project_path.clone();
    let prompt = req.prompt.clone();
    let model = req.model;
    let permission_mode = req.permission_mode;
    let max_budget = req.max_budget_usd;

    let handle = tokio::spawn({
        let task_handles = task_handles.clone();
        let job_id_inner = job_id_clone.clone();
        async move {
            let result = run_swarm_task(
                &job_id_inner,
                &project_path,
                &prompt,
                &model,
                &permission_mode,
                max_budget,
            )
            .await;

            let mut jobs = jobs.lock().await;
            if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id_inner) {
                // Don't clobber a status the cancel handler already set.
                let already_terminal = job.status != "running";
                match result {
                    Ok(outcome) => {
                        if !already_terminal {
                            job.status = "succeeded".to_string();
                        }
                        job.cost_usd = outcome.cost_usd;
                        job.tool_calls = outcome.tool_calls;
                        job.input_tokens = outcome.input_tokens;
                        job.output_tokens = outcome.output_tokens;
                        job.summary = outcome.summary;
                        job.branch = outcome.branch;
                        job.changed_files = outcome.changed_files;
                    }
                    Err(e) => {
                        if !already_terminal {
                            job.status = "failed".to_string();
                            job.error = Some(e);
                        }
                    }
                }
                if job.finished_at.is_none() {
                    job.finished_at = Some(
                        chrono::Local::now()
                            .format("%Y-%m-%dT%H:%M:%SZ")
                            .to_string(),
                    );
                }
            }
            // Self-cleanup so cancel can't abort a finished handle later.
            task_handles.lock().await.remove(&job_id_inner);
        }
    });
    task_handles.lock().await.insert(job_id_clone, handle);

    Json(serde_json::json!({ "job_id": job_id }))
}

pub async fn agent_cancel(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let handle = state.task_handles.lock().await.remove(&id);
    let Some(handle) = handle else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "Job not running or already finished"
        }));
    };
    handle.abort();

    let mut jobs = state.jobs.lock().await;
    let Some(job) = jobs.iter_mut().find(|j| j.id == id) else {
        return Json(serde_json::json!({
            "ok": true,
            "warning": "Task aborted but no job record found"
        }));
    };
    if job.status == "running" {
        job.status = "cancelled".to_string();
        job.finished_at = Some(
            chrono::Local::now()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    Json(serde_json::json!({ "ok": true, "id": id, "status": job.status }))
}

struct SwarmOutcome {
    cost_usd: f64,
    tool_calls: u32,
    input_tokens: u64,
    output_tokens: u64,
    summary: Option<String>,
    branch: Option<String>,
    changed_files: Vec<String>,
}

async fn run_swarm_task(
    job_id: &str,
    project_path: &str,
    prompt: &str,
    model: &str,
    permission_mode: &str,
    max_budget: f64,
) -> Result<SwarmOutcome, String> {
    use swarm::config::*;
    use swarm::domain::*;
    use swarm::orchestrator::Orchestrator;

    let task = Task {
        spec: TaskSpec {
            id: job_id.to_string(),
            title: Some(prompt.chars().take(50).collect()),
            prompt: prompt.to_string(),
            task_type: TaskType::Implement,
            depends_on: vec![],
            priority: Priority::Normal,
            metadata: serde_json::Value::Null,
            backend_ref: None,
        },
        policy: TaskPolicy {
            retry_policy: RetryPolicy {
                max_retries: 0,
                retry_on_timeout: false,
                retry_on_cli_error: false,
            },
            failure_policy: FailurePolicy::SkipDependents,
            cleanup_policy: CleanupPolicy::OnSuccess,
            timeout_action: TimeoutAction::FailImmediately,
            budget_action: BudgetAction::CancelTask,
        },
        execution: TaskExecutionConfig {
            allowed_tools: vec![
                "Read".into(),
                "Edit".into(),
                "Write".into(),
                "Bash".into(),
                "Glob".into(),
                "Grep".into(),
            ],
            system_prompt_append: None,
            model: Some(model.to_string()),
            permission_mode: Some(permission_mode.to_string()),
            timeout_seconds: Some(1800),
            max_budget_usd: Some(max_budget),
        },
        output: TaskOutputConfig {
            commit: false,
            commit_message: None,
        },
    };

    // Detect the project's current/default branch
    let base_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(project_path)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "main".to_string());

    let config = SwarmConfig {
        scheduler: SchedulerConfig {
            max_concurrent: 1,
            base_branch,
            ..Default::default()
        },
        agent: AgentConfig {
            default_model: model.to_string(),
            default_permission_mode: permission_mode.to_string(),
            ..Default::default()
        },
        backend: BackendConfig {
            toml: TomlBackendConfig {
                state_path: PathBuf::from(project_path).join(".swarm/state.json"),
                logs_path: PathBuf::from(project_path).join(".swarm/logs"),
                ..Default::default()
            },
            ..Default::default()
        },
        defaults: DefaultsConfig::default(),
        tasks: vec![task.clone()],
    };

    let graph = TaskGraph::new(vec![task]).map_err(|e| format!("{}", e))?;
    let orchestrator = Orchestrator::new(project_path, config).map_err(|e| format!("{}", e))?;
    let snapshot = orchestrator
        .run_graph(graph)
        .await
        .map_err(|e| format!("{}", e))?;

    // Extract results from snapshot
    if let Some(record) = snapshot.tasks.get(job_id) {
        let result = record.result.as_ref();
        Ok(SwarmOutcome {
            cost_usd: record.estimated_cost_usd.unwrap_or(0.0),
            tool_calls: record.tool_calls,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            summary: result.and_then(|r| r.summary.clone()),
            branch: record.branch.clone(),
            changed_files: result.map(|r| r.changed_files.clone()).unwrap_or_default(),
        })
    } else {
        Err("Task not found in snapshot".to_string())
    }
}

pub async fn agent_jobs(State(state): State<AppState>) -> Json<Vec<AgentJob>> {
    let jobs = state.jobs.lock().await;
    Json(jobs.clone())
}

pub async fn agent_job_detail(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let jobs = state.jobs.lock().await;
    if let Some(job) = jobs.iter().find(|j| j.id == id) {
        Json(serde_json::to_value(job).unwrap_or_default())
    } else {
        Json(serde_json::json!({ "error": "Job not found" }))
    }
}

pub async fn agent_job_log(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> String {
    let jobs = state.jobs.lock().await;
    if let Some(job) = jobs.iter().find(|j| j.id == id) {
        let log_path = PathBuf::from(&job.project_path)
            .join(".swarm/logs")
            .join(format!("{}.log", id));
        std::fs::read_to_string(&log_path).unwrap_or_else(|_| "No log available yet.".to_string())
    } else {
        "Job not found".to_string()
    }
}
