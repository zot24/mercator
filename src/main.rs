//! Mercator — CLI parsing, HTTP handlers, and the wiring between them.
//!
//! What lives here, what doesn't:
//!
//! - **CLI**: `clap`-derived `Cli` / `Commands` enums + the per-subcommand
//!   match arms in `main()`. Subcommands today: `survey`, `serve`,
//!   `export`, `list`, `search`.
//! - **HTTP**: `Axum` route registration, `AppState` (holds the live
//!   `rusqlite::Connection` behind a `tokio::sync::Mutex`), the
//!   `MERCATOR_TOKEN` middleware, and every `/api/*` handler.
//! - **Glue**: opens the DB once at startup (Survey + Serve), runs the
//!   one-shot JSON-to-SQLite import, hands the connection to the route
//!   handlers via `State<AppState>`.
//!
//! Domain logic lives in dedicated modules — see them for what they own:
//!
//! - [`db`] — SQLite schema, migrations, CRUD, FTS5 search/list
//! - [`project`] — `Project` struct + JSON load/save (legacy snapshot path)
//! - [`sources`] — local FS survey, GitHub/GitLab fetchers, Obsidian, dedup, `Source` trait
//! - [`markdown`] — description extraction, frontmatter, export rendering
//! - [`tags_graph`] — auto-tagging + D3 graph computation
//! - [`skills`] — skills inventory walker
//! - `agent` — swarm-feature agent runner (cfg-gated; intra-doc link
//!   omitted because the module is conditional on `--features swarm`)
//!
//! See [`docs/STATUS.md`](../../docs/STATUS.md) for the live snapshot of
//! state and roadmap, and [`CLAUDE.md`](../../CLAUDE.md) for the
//! operator's manual.

#[cfg(feature = "swarm")]
mod agent;
mod config;
mod db;
mod markdown;
mod project;
mod readme;
mod skills;
mod sources;
mod tags_graph;
mod ticket;

#[cfg(feature = "swarm")]
use crate::agent::AgentJob;

use crate::markdown::run_export;
use crate::project::{format_time, load_map, save_map, Project, ProjectType};
use crate::sources::{
    deduplicate_projects, link_obsidian_notes, scan_obsidian_vault, survey_projects, AnySource,
    GitHubSource, GitLabSource,
};
use crate::tags_graph::{auto_tag_projects, compute_graph};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;

#[derive(Parser)]
#[command(name = "mercator")]
#[command(version)]
#[command(about = "Cartography for your local projects", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Output format for the `list` and `search` subcommands. `Text` is the
/// tab-separated row format that has been stable since #25 shipped — safe
/// for `awk` / `cut` / `grep` pipelines. `Json` is a JSON array of full
/// `Project` records (same Serde shape as `/api/map`), so callers can pipe
/// straight into `jq` and access fields the tab format drops (description,
/// gitBranch, lastCommit, remoteUrl, agentUsed, …).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Subcommand, Debug)]
enum ActiveCommand {
    /// Mark a path as currently active. Re-adding refreshes the
    /// timestamp and replaces the optional note.
    Add {
        /// Absolute or relative path; trailing slash is trimmed. The
        /// path is *not* required to exist in the projects table — the
        /// active set is orthogonal to surveyed state.
        path: PathBuf,
        /// Free-form note attached to the active entry (e.g. "shipping
        /// active export"). Visible to consumers of the JSON export.
        #[arg(long, short = 'n')]
        note: Option<String>,
    },
    /// Remove a path from the active list. No-op if the path isn't on
    /// the list (exit 0, prints a warning to stderr).
    Remove {
        /// Path to deactivate. Trailing slash trimmed for parity with
        /// `add`.
        path: PathBuf,
    },
    /// Print the active list. Default output is tab-separated
    /// (path, activated_at, note) ordered most-recent-first; use
    /// `--format json` for a richer shape that joins in project
    /// metadata when known (description, type, tech_stack, tags).
    List {
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Re-write the JSON snapshot from the current DB state without
    /// changing the active set. Useful when project metadata changed
    /// (e.g. after a re-survey) and you want Hermes to see the
    /// updated description/tags.
    Export,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // CLI args parsed once at startup; size doesn't matter
enum Commands {
    /// Survey a directory for projects
    Survey {
        /// One or more paths to survey (default: current directory). Repeat
        /// the positional arg to scan multiple roots in a single run, e.g.
        /// `mercator survey ~/code ~/work/repos ~/oss`.
        #[arg(num_args = 0..)]
        paths: Vec<PathBuf>,

        /// Output file for the survey results
        #[arg(short, long, default_value = "mercator_map.json")]
        output: PathBuf,

        /// GitHub username to fetch repos from
        #[arg(long)]
        github: Option<String>,

        /// GitHub personal access token (falls back to GITHUB_TOKEN env)
        /// — required for private repos and lifts the unauthenticated 60/hr
        /// rate limit to 5000/hr
        #[arg(long, env = "GITHUB_TOKEN", hide_env_values = true)]
        github_token: Option<String>,

        /// GitLab username to fetch repos from
        #[arg(long)]
        gitlab: Option<String>,

        /// GitLab personal access token (falls back to GITLAB_TOKEN env)
        #[arg(long, env = "GITLAB_TOKEN", hide_env_values = true)]
        gitlab_token: Option<String>,

        /// Cap the number of repos fetched per remote source (default: no cap)
        #[arg(long)]
        max_repos: Option<usize>,

        /// Re-scan every N minutes (runs in foreground)
        #[arg(short, long)]
        watch: Option<u64>,

        /// Path to an Obsidian vault (e.g. ~/Desktop/brain)
        #[arg(long)]
        obsidian: Option<PathBuf>,

        /// Projects folder within Obsidian vault (default: "Projects")
        #[arg(long, default_value = "Projects")]
        obsidian_folder: String,

        /// Obsidian vault name for obsidian:// URIs (default: inferred from vault dir name)
        #[arg(long)]
        obsidian_vault: Option<String>,

        /// Run `ob sync` before scanning the Obsidian vault (for Docker/remote)
        #[arg(long)]
        obsidian_sync: bool,

        /// SQLite database file. Stage 1 of #24: the DB is populated as a
        /// parallel store; the JSON map is still the source of truth for
        /// dashboard reads. Re-running `survey` re-imports the resulting
        /// JSON into this DB so the user can verify the migration.
        #[arg(short = 'd', long, default_value = "mercator.db")]
        db: PathBuf,
    },
    /// List projects from the DB, optionally filtered by type / tag / tech.
    /// Default output is one project per line, tab-separated columns (type,
    /// path, name, tags-comma-joined, tech-comma-joined) so you can pipe to
    /// `awk` / `cut` / `grep`. Use `--format json` for a JSON array of full
    /// project records (matches the `/api/map` shape). Closes #25 alongside
    /// `search`.
    List {
        /// SQLite database file
        #[arg(short = 'd', long, default_value = "mercator.db")]
        db: PathBuf,

        /// Filter by project type (Git, Folder, Idea, GitHub, GitLab, Obsidian)
        #[arg(short = 't', long = "type")]
        project_type: Option<String>,

        /// Filter by tag (exact match — case-sensitive)
        #[arg(long)]
        tag: Option<String>,

        /// Filter by tech-stack entry (e.g. "Rust", "Node.js")
        #[arg(long)]
        tech: Option<String>,

        /// Show only projects on the active list (see `mercator active`).
        /// AND-combined with the other filters.
        #[arg(long)]
        active: bool,

        /// Show only projects that are not git repositories (the local
        /// Folder / Idea types) — directories worth putting under version
        /// control. AND-combined with the other filters.
        #[arg(long = "no-git")]
        no_git: bool,

        /// Show only projects with no git remote configured (covers both
        /// plain folders and git repos that have no `origin` remote — i.e.
        /// anything not pushed anywhere). AND-combined with the other filters.
        #[arg(long = "no-remote")]
        no_remote: bool,

        /// Show only git projects whose branch has diverged from its
        /// upstream — ahead (unpushed) and/or behind (unpulled). Reflects
        /// the last fetch; `mercator survey` does not fetch. AND-combined
        /// with the other filters.
        #[arg(long = "out-of-sync")]
        out_of_sync: bool,

        /// Output format: `text` (default — an aligned table on a terminal,
        /// tab-separated rows when piped so shell pipelines keep working)
        /// or `json` (a JSON array of full project records, matching the
        /// `/api/map` shape — feed to `jq`, scripts, or other tools).
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Manage the "currently working on" list. The active set is
    /// orthogonal to surveyed state — it survives re-surveys, and a path
    /// can be activated before being surveyed. Every mutation writes
    /// `active-projects.json` (next to the DB by default) so external
    /// agents (Hermes, session-loaders) can pick up context without
    /// shelling into the DB.
    Active {
        #[command(subcommand)]
        action: ActiveCommand,

        /// SQLite database file
        #[arg(short = 'd', long, default_value = "mercator.db", global = true)]
        db: PathBuf,

        /// Path the JSON snapshot is auto-written to after every
        /// add/remove. Defaults to `active-projects.json` next to the
        /// DB. Pass `--export -` (or any path containing `/dev/null`)
        /// to suppress.
        #[arg(long, global = true)]
        export: Option<PathBuf>,
    },
    /// Full-text search projects by name, description, and tags. Each
    /// whitespace-separated token must match (AND); punctuation inside
    /// a word is literal so `cli-tool` finds the project named
    /// `cli-tool`. Backed by SQLite FTS5.
    ///
    /// Note: tech_stack is not full-text-indexed; use `mercator list
    /// --tech Rust` to filter by tech-stack entry.
    /// Closes #25 alongside `list`.
    Search {
        /// FTS5 query string. Quote multi-word queries from the shell.
        query: String,

        /// SQLite database file
        #[arg(short = 'd', long, default_value = "mercator.db")]
        db: PathBuf,

        /// Output format: `text` (default, tab-separated for shell pipelines)
        /// or `json` (a JSON array of full project records, matching the
        /// `/api/map` shape — feed to `jq`, scripts, or other tools).
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Export the map as one markdown file per project (one folder of
    /// structured notes that any other tool can consume)
    Export {
        /// Output directory (created if missing). Defaults to `./mercator-export`.
        #[arg(default_value = "mercator-export")]
        out_dir: PathBuf,

        /// SQLite database file to read from. Stage 3b of #24: export
        /// reads the live DB instead of the legacy `mercator_map.json`
        /// snapshot, so dashboard-side purges since the last `mercator
        /// survey` are honored.
        #[arg(short = 'd', long, default_value = "mercator.db")]
        db: PathBuf,

        /// When set, write under `<vault>/<folder>/` instead of `out_dir`.
        /// Feed the Obsidian LLM-wiki layer (issue #22).
        #[arg(long)]
        obsidian_vault: Option<PathBuf>,

        /// Subdirectory inside the Obsidian vault (default: "Projects")
        #[arg(long, default_value = "Projects")]
        obsidian_folder: String,
    },
    /// Generate a Markdown "projects" section (the active set by default) for a
    /// profile README, and optionally splice it into a file between
    /// `<!-- MERCATOR:START -->` / `<!-- MERCATOR:END -->` markers. Without
    /// `--inject` the block is printed to stdout.
    Readme {
        /// SQLite database file
        #[arg(short = 'd', long, default_value = "mercator.db")]
        db: PathBuf,

        /// Update this file in place, replacing the content between the
        /// mercator markers (appends a fresh block if the markers are absent).
        /// A missing file is created.
        #[arg(long)]
        inject: Option<PathBuf>,

        /// Include every project instead of just the active set
        /// (see `mercator active`).
        #[arg(long)]
        all: bool,

        /// Filter by project type (Git, Folder, Idea, GitHub, GitLab, Obsidian)
        #[arg(short = 't', long = "type")]
        project_type: Option<String>,

        /// Filter by tag (exact match — case-sensitive)
        #[arg(long)]
        tag: Option<String>,

        /// Filter by tech-stack entry (e.g. "Rust", "Node.js")
        #[arg(long)]
        tech: Option<String>,

        /// Cap the number of projects rendered
        #[arg(long)]
        limit: Option<usize>,

        /// Section heading (default: "🛠️ What I'm working on")
        #[arg(long)]
        title: Option<String>,

        /// Omit the "mapped by mercator" badge/footer
        #[arg(long = "no-badge")]
        no_badge: bool,
    },
    /// Start the visualization server
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value_t = 3000)]
        port: u16,

        /// IP address to bind to (use 0.0.0.0 for network access)
        #[arg(short, long, default_value = "127.0.0.1")]
        bind: IpAddr,

        /// Path to the map JSON file
        #[arg(short, long, default_value = "mercator_map.json")]
        map_file: PathBuf,

        /// SQLite database file. Stage 1 of #24: the DB is populated from
        /// the JSON map on startup; dashboard reads still go through JSON.
        #[arg(short = 'd', long, default_value = "mercator.db")]
        db: PathBuf,

        /// Local paths to re-scan when the dashboard's refresh button is
        /// clicked. Pass once per root: `--refresh ~/code --refresh ~/oss`.
        /// When empty, the refresh button just reloads the page (legacy
        /// behaviour). Remote sources (GitHub/GitLab/Obsidian) are not
        /// re-fetched on refresh — run `mercator survey ...` for those.
        #[arg(long)]
        refresh: Vec<PathBuf>,
    },
}

// (Project, ProjectType, format_time moved to src/project.rs)

// (sources — survey, GitHub, GitLab, Obsidian, dedup — moved to src/sources.rs)

// (export rendering moved to src/markdown.rs)

// (save_map and load_map moved to src/project.rs)

#[derive(Deserialize)]
struct OpenTerminalRequest {
    agent: String,
    path: String,
}

#[derive(Serialize)]
struct OpenTerminalResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn open_terminal(Json(req): Json<OpenTerminalRequest>) -> Json<OpenTerminalResponse> {
    let agent_cmd = match req.agent.as_str() {
        "claude" => "claude",
        "codex" => "codex",
        _ => {
            return Json(OpenTerminalResponse {
                success: false,
                error: Some("Unknown agent. Use 'claude' or 'codex'.".to_string()),
            });
        }
    };

    // Use osascript to open a new Terminal window and run the command
    let script = format!(
        r#"tell application "Terminal"
            activate
            do script "cd '{}' && {}"
        end tell"#,
        req.path.replace('\'', "'\\''"),
        agent_cmd,
    );

    match Command::new("osascript").arg("-e").arg(&script).output() {
        Ok(output) if output.status.success() => Json(OpenTerminalResponse {
            success: true,
            error: None,
        }),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Json(OpenTerminalResponse {
                success: false,
                error: Some(stderr),
            })
        }
        Err(e) => Json(OpenTerminalResponse {
            success: false,
            error: Some(format!("Failed to run osascript: {}", e)),
        }),
    }
}

// (markdown helpers moved to src/markdown.rs)

/// API endpoint: get git status for a project path
#[derive(Deserialize)]
struct GitStatusQuery {
    path: String,
}

/// Parse a `git status --short` line and return the relative path. Skips
/// the two-character status prefix and any leading whitespace, and unwraps
/// rename arrows like `R  old -> new`. Pure function, unit-tested.
fn parse_git_status_path(line: &str) -> Option<&str> {
    let trimmed = line.get(3..)?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((_, after)) = trimmed.split_once(" -> ") {
        Some(after)
    } else {
        Some(trimmed)
    }
}

async fn get_git_status_api(
    axum::extract::Query(q): axum::extract::Query<GitStatusQuery>,
) -> Json<serde_json::Value> {
    let output = Command::new("git")
        .args(["status", "--short"])
        .current_dir(&q.path)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let files: Vec<serde_json::Value> = stdout
                .lines()
                .map(|line| {
                    let rel = parse_git_status_path(line);
                    let mtime = rel
                        .map(|r| Path::new(&q.path).join(r))
                        .and_then(|p| std::fs::metadata(&p).ok())
                        .and_then(|m| m.modified().ok())
                        .map(format_time);
                    serde_json::json!({
                        "raw": line,
                        "path": rel,
                        "mtime": mtime,
                    })
                })
                .collect();
            Json(serde_json::json!({ "files": files }))
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Json(serde_json::json!({ "error": stderr.to_string() }))
        }
        Err(e) => Json(serde_json::json!({ "error": format!("{}", e) })),
    }
}

// (auto_tag_projects, domain_keywords, compute_graph moved to src/tags_graph.rs)

// (agent runner — swarm-feature-gated — moved to src/agent.rs)

#[derive(Clone)]
struct AppState {
    #[cfg(feature = "swarm")]
    jobs: Arc<Mutex<Vec<AgentJob>>>,
    /// JoinHandles for in-flight agent tasks, keyed by job id. A `cancel`
    /// request looks the handle up here and `.abort()`s it. The spawned
    /// task removes its own entry on completion.
    #[cfg(feature = "swarm")]
    task_handles: Arc<Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>>,
    map_file: PathBuf,
    /// Live SQLite handle. Stage 2a of #24 routes `/api/map` and
    /// `/api/graph` through here, with a JSON fallback if the DB read
    /// fails. Wrapped in a tokio Mutex because `rusqlite::Connection` is
    /// `!Sync`; lock holds are short (a single SELECT).
    db: Arc<Mutex<rusqlite::Connection>>,
    /// Paths the dashboard's refresh button re-scans. Empty = refresh is a
    /// no-op (button just reloads the page). Configured via `serve --refresh`.
    refresh_paths: Vec<PathBuf>,
    /// User-level config — GitHub/GitLab user names + tokens. Loaded
    /// from `~/.config/mercator/config.toml` once at startup. The
    /// dashboard's refresh handler consults it to decide whether to
    /// fetch remote sources (#2). Behind a Mutex so a future
    /// `POST /api/settings` can mutate it in place without restart.
    cfg: Arc<Mutex<config::Config>>,
}

// ── Project Preview (file tree + viewer) ───────────────────────────────

const PREVIEW_MAX_DEPTH: usize = 6;
const PREVIEW_FILE_BYTES: u64 = 1024 * 1024;
const PREVIEW_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".swarm",
    ".cache",
];

#[derive(Serialize)]
struct FileNode {
    name: String,
    path: String,
    is_dir: bool,
    /// ISO 8601 modified timestamp; None if metadata couldn't be read
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<FileNode>>,
}

fn walk_tree(p: &Path, depth: usize) -> Vec<FileNode> {
    if depth >= PREVIEW_MAX_DEPTH {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(p) else {
        return Vec::new();
    };
    let mut nodes: Vec<FileNode> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if PREVIEW_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if name.starts_with('.')
            && name != ".gitignore"
            && name != ".env.example"
            && name != ".github"
        {
            continue;
        }
        let path = e.path();
        let is_dir = path.is_dir();
        let mtime = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(format_time);
        let children = if is_dir {
            Some(walk_tree(&path, depth + 1))
        } else {
            None
        };
        nodes.push(FileNode {
            name,
            path: path.to_string_lossy().into_owned(),
            is_dir,
            mtime,
            children,
        });
    }
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    nodes
}

#[derive(Deserialize)]
struct TreeQuery {
    path: String,
}

async fn project_tree_api(
    axum::extract::Query(q): axum::extract::Query<TreeQuery>,
) -> Json<serde_json::Value> {
    let root = PathBuf::from(&q.path);
    if !root.is_dir() {
        return Json(serde_json::json!({ "error": "Not a directory" }));
    }
    let canonical_root = std::fs::canonicalize(&root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| q.path.clone());
    let nodes = walk_tree(&root, 0);
    Json(serde_json::json!({
        "root": canonical_root,
        "tree": nodes,
    }))
}

#[derive(Deserialize)]
struct FileQuery {
    root: String,
    path: String,
}

async fn project_file_api(
    axum::extract::Query(q): axum::extract::Query<FileQuery>,
) -> Json<serde_json::Value> {
    let root = match std::fs::canonicalize(&q.root) {
        Ok(p) => p,
        Err(_) => return Json(serde_json::json!({ "error": "Invalid root" })),
    };
    let file = match std::fs::canonicalize(&q.path) {
        Ok(p) => p,
        Err(_) => return Json(serde_json::json!({ "error": "File not found" })),
    };
    if !file.starts_with(&root) {
        return Json(serde_json::json!({ "error": "Access denied (path traversal)" }));
    }
    let metadata = match std::fs::metadata(&file) {
        Ok(m) => m,
        Err(e) => return Json(serde_json::json!({ "error": format!("{}", e) })),
    };
    if metadata.is_dir() {
        return Json(serde_json::json!({ "error": "Path is a directory" }));
    }
    if metadata.len() > PREVIEW_FILE_BYTES {
        return Json(serde_json::json!({ "error": "File too large", "size": metadata.len() }));
    }
    match std::fs::read_to_string(&file) {
        Ok(c) => Json(serde_json::json!({ "content": c, "size": metadata.len() })),
        Err(_) => {
            Json(serde_json::json!({ "error": "Binary or non-UTF8 file", "size": metadata.len() }))
        }
    }
}

/// Path to the purge blocklist that lives next to the map file
fn purge_file_path(map_file: &Path) -> PathBuf {
    let parent = map_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("mercator_purged.json")
}

fn read_purged(map_file: &Path) -> std::collections::HashSet<String> {
    let path = purge_file_path(map_file);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    serde_json::from_str::<Vec<String>>(&content)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// `GET /api/map` — return the full project list.
///
/// Stage 2a of #24: read from SQLite, fall back to JSON only if the DB
/// read fails. The fallback exists because `mercator survey` writes JSON
/// first today; the DB catches up via the import call inside the same
/// survey run, but a bug in that path shouldn't blank the dashboard.
async fn serve_map_api(State(state): State<AppState>) -> Json<Vec<Project>> {
    let conn = state.db.lock().await;
    match db::load_all_projects(&conn) {
        Ok(projects) => Json(projects),
        Err(db_err) => {
            eprintln!("Warning: db read failed ({}); falling back to JSON", db_err);
            match load_map(&state.map_file) {
                Ok(projects) => Json(projects),
                Err(json_err) => {
                    eprintln!("Warning: {}", json_err);
                    Json(vec![])
                }
            }
        }
    }
}

/// `GET /api/graph` — return the tag co-occurrence graph derived from
/// the current project list. Same DB-then-JSON fallback as `/api/map`.
async fn serve_graph_api(State(state): State<AppState>) -> Json<serde_json::Value> {
    let conn = state.db.lock().await;
    match db::load_all_projects(&conn) {
        Ok(projects) => Json(compute_graph(&projects)),
        Err(_) => match load_map(&state.map_file) {
            Ok(projects) => Json(compute_graph(&projects)),
            Err(_) => Json(serde_json::json!({ "nodes": [], "edges": [] })),
        },
    }
}

#[derive(Deserialize)]
struct PurgeRequest {
    path: String,
}

async fn purge_project_api(
    State(state): State<AppState>,
    Json(req): Json<PurgeRequest>,
) -> Json<serde_json::Value> {
    let target = req.path.trim_end_matches('/').to_string();

    // Source of truth: the DB. We delete the projects row + insert into
    // purged in a single transaction so the dashboard can never see a
    // half-purged state.
    let (removed_count, purged_total) = {
        let mut conn = state.db.lock().await;
        match db::purge_project(&mut conn, &target) {
            Ok((_, project_was_present)) => {
                let remaining = db::count_purged(&conn).unwrap_or(0);
                (usize::from(project_was_present), remaining as usize)
            }
            Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
        }
    };
    let remaining = {
        let conn = state.db.lock().await;
        db::count_projects(&conn).unwrap_or(0) as usize
    };

    Json(serde_json::json!({
        "ok": true,
        "removed": removed_count,
        "remaining": remaining,
        "purged_total": purged_total,
    }))
}

async fn purged_list_api(State(state): State<AppState>) -> Json<Vec<String>> {
    let conn = state.db.lock().await;
    match db::list_purged(&conn) {
        Ok(list) => Json(list),
        Err(e) => {
            eprintln!(
                "Warning: db purged list failed ({}); falling back to JSON",
                e
            );
            let mut list: Vec<String> = read_purged(&state.map_file).into_iter().collect();
            list.sort();
            Json(list)
        }
    }
}

#[derive(Deserialize)]
struct RestoreRequest {
    path: String,
}

async fn restore_project_api(
    State(state): State<AppState>,
    Json(req): Json<RestoreRequest>,
) -> Json<serde_json::Value> {
    let target = req.path.trim_end_matches('/').to_string();

    let (removed, remaining) = {
        let conn = state.db.lock().await;
        match db::restore_purged(&conn, &target) {
            Ok(was_present) => (was_present, db::count_purged(&conn).unwrap_or(0) as usize),
            Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
        }
    };

    Json(serde_json::json!({
        "ok": true,
        "removed_from_blocklist": removed,
        "remaining": remaining,
    }))
}

/// API endpoint: re-survey the configured local paths and re-fetch any
/// remote sources the user has set up in `~/.config/mercator/config.toml`.
/// Behavior matches `mercator survey` for the configured providers, so
/// the dashboard's refresh button is now strictly more capable than
/// before — it picks up remote changes without dropping to the CLI.
async fn refresh_survey_api(State(state): State<AppState>) -> Json<serde_json::Value> {
    if state.refresh_paths.is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": "No refresh paths configured. Restart with `serve --refresh <path>` to enable.",
        }));
    }
    let mut all = Vec::new();
    let mut per_path = Vec::new();
    for path in &state.refresh_paths {
        let found = survey_projects(path);
        per_path.push(serde_json::json!({
            "path": path.to_string_lossy(),
            "found": found.len(),
        }));
        all.extend(found);
    }

    // Build remote-source list from the loaded config — same shape as
    // the `mercator survey` CLI, just sourced from the config file
    // instead of CLI flags. Token + user are owned (cloned) so the
    // futures don't borrow the lock.
    let remote_sources: Vec<sources::AnySource> = {
        let cfg = state.cfg.lock().await;
        let mut v = Vec::new();
        if let Some(user) = cfg.github.user() {
            v.push(sources::AnySource::GitHub(sources::GitHubSource {
                username: user.to_string(),
                token: cfg.github.token().map(str::to_string),
                max_repos: None,
            }));
        }
        if let Some(user) = cfg.gitlab.user() {
            v.push(sources::AnySource::GitLab(sources::GitLabSource {
                username: user.to_string(),
                token: cfg.gitlab.token().map(str::to_string),
                max_repos: None,
            }));
        }
        v
    };
    let mut remote_per_source = Vec::new();
    if !remote_sources.is_empty() {
        let results = futures::future::join_all(remote_sources.iter().map(|s| s.fetch())).await;
        for (source, result) in remote_sources.iter().zip(results) {
            let (count, error) = match result {
                Ok(repos) => {
                    let n = repos.len();
                    all.extend(repos);
                    (n, None)
                }
                Err(e) => (0, Some(e.to_string())),
            };
            remote_per_source.push(serde_json::json!({
                "source": source.name(),
                "fetched": count,
                "error": error,
            }));
        }
    }

    // Honour the existing purge blocklist (DB is the source of truth;
    // fall back to the JSON sidecar so a transient DB hiccup doesn't
    // re-introduce a project the user explicitly purged).
    let purged: std::collections::HashSet<String> = {
        let conn = state.db.lock().await;
        match db::list_purged(&conn) {
            Ok(list) => list.into_iter().collect(),
            Err(_) => read_purged(&state.map_file),
        }
    };
    all.retain(|p| !purged.contains(p.path.trim_end_matches('/')));
    let mut all = deduplicate_projects(all);
    auto_tag_projects(&mut all);

    {
        let mut conn = state.db.lock().await;
        if let Err(e) = db::upsert_projects(&mut conn, &all) {
            return Json(serde_json::json!({ "ok": false, "error": e }));
        }
    }
    Json(serde_json::json!({
        "ok": true,
        "total": all.len(),
        "per_path": per_path,
        "per_source": remote_per_source,
    }))
}

/// `GET /api/settings` — return the current config minus secrets.
/// User names + a `*_token_set` boolean are exposed; raw tokens are
/// not. The dashboard uses this to render a "you have a token
/// configured" hint without ever holding the secret in browser state.
async fn settings_api(State(state): State<AppState>) -> Json<config::RedactedConfig> {
    let cfg = state.cfg.lock().await;
    Json(cfg.redacted())
}

/// `POST /api/settings` — update user-level config from the dashboard's
/// settings panel. Fields are independent and sparse:
///
/// - `*_user`: if the field is present (even empty), the user is set to
///   that value. Absent = no change.
/// - `*_token`: if the field is present and **non-empty**, the token is
///   replaced. Absent or empty = no change. This is what lets the
///   dashboard show "(saved — leave blank to keep)" without accidentally
///   clearing the token when the user only wanted to update their
///   username.
/// - `*_clear_token`: if `true`, the token is cleared regardless of
///   what `*_token` holds. Explicit opt-in for the destructive action.
///
/// On success the redacted config is returned (same shape as `GET`),
/// so the frontend can refresh its view without a second round-trip.
#[derive(Deserialize, Default)]
struct SettingsUpdate {
    #[serde(default)]
    github_user: Option<String>,
    #[serde(default)]
    github_token: Option<String>,
    #[serde(default)]
    github_clear_token: bool,
    #[serde(default)]
    gitlab_user: Option<String>,
    #[serde(default)]
    gitlab_token: Option<String>,
    #[serde(default)]
    gitlab_clear_token: bool,
}

async fn settings_update_api(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Json<serde_json::Value> {
    let mut cfg = state.cfg.lock().await;

    // Apply per-field updates. Strings are trimmed on input — leading/
    // trailing whitespace in a token is almost always a paste artifact,
    // and rejecting it silently here saves a round-trip of confusion.
    if let Some(u) = update.github_user {
        cfg.github.user = Some(u.trim().to_string()).filter(|s| !s.is_empty());
    }
    if update.github_clear_token {
        cfg.github.token = None;
    } else if let Some(t) = update.github_token {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            cfg.github.token = Some(trimmed.to_string());
        }
    }
    if let Some(u) = update.gitlab_user {
        cfg.gitlab.user = Some(u.trim().to_string()).filter(|s| !s.is_empty());
    }
    if update.gitlab_clear_token {
        cfg.gitlab.token = None;
    } else if let Some(t) = update.gitlab_token {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            cfg.gitlab.token = Some(trimmed.to_string());
        }
    }

    if let Err(e) = config::save_to(&config::config_path(), &cfg) {
        return Json(serde_json::json!({ "ok": false, "error": e }));
    }
    Json(serde_json::json!({ "ok": true, "settings": cfg.redacted() }))
}

async fn recategorize_api(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Read the live state from the DB — it's been the source of truth
    // since stage 2a, and we want recategorize to see whatever purges /
    // restores have happened.
    let mut projects = {
        let conn = state.db.lock().await;
        match db::load_all_projects(&conn) {
            Ok(p) => p,
            Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
        }
    };
    auto_tag_projects(&mut projects);
    {
        let mut conn = state.db.lock().await;
        if let Err(e) = db::upsert_projects(&mut conn, &projects) {
            return Json(serde_json::json!({ "ok": false, "error": e }));
        }
    }
    let count = projects.len();
    let tagged = projects.iter().filter(|p| !p.tags.is_empty()).count();
    Json(serde_json::json!({ "ok": true, "projects": count, "tagged": tagged }))
}

// (skills inventory moved to src/skills.rs)

async fn skills_api(State(state): State<AppState>) -> Json<Vec<crate::skills::SkillGroup>> {
    Json(crate::skills::compute_skill_groups(&state.map_file))
}

/// Tab-separated single-line project row for `mercator list` / `search`.
/// Columns: type, path, name, tags (comma-joined), tech (comma-joined).
/// Stable for shell pipelines (`awk -F'\t'`, `cut -f`, `grep`).
fn format_project_row(p: &Project) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}",
        project_type_label(&p.project_type),
        p.path,
        p.name,
        p.tags.join(","),
        p.tech_stack.join(","),
    )
}

/// Human-facing label for a `ProjectType`. Same strings the tab-separated
/// row and the JSON `type` field use, so the three surfaces stay in step.
fn project_type_label(t: &ProjectType) -> &'static str {
    match t {
        ProjectType::Git => "Git",
        ProjectType::Folder => "Folder",
        ProjectType::Idea => "Idea",
        ProjectType::GitHub => "GitHub",
        ProjectType::GitLab => "GitLab",
        ProjectType::Obsidian => "Obsidian",
    }
}

// ── Pretty `--format text` table ───────────────────────────────────────
//
// `mercator list` / `search` default to `--format text`. On an interactive
// terminal we render an aligned, lightly-coloured table (the old raw
// tab-separated output read as noise once paths got long). When stdout is
// *not* a TTY we fall back to [`format_project_row`] so existing
// `awk` / `cut` / `grep` pipelines keep seeing the stable tab format.

const C_RESET: &str = "\x1b[0m";
const C_BOLD: &str = "\x1b[1m";
const C_DIM: &str = "\x1b[2m";
const C_GREEN: &str = "\x1b[32m";
const C_YELLOW: &str = "\x1b[33m";
const C_CYAN: &str = "\x1b[36m";

/// Render the chosen output format for a project list. Text output is
/// TTY-aware: aligned table when interactive, tab-separated rows when
/// piped. JSON is unconditional.
fn print_project_list(projects: &[Project], format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", format_projects_json(projects)),
        OutputFormat::Text => {
            use std::io::IsTerminal;
            if std::io::stdout().is_terminal() {
                // `NO_COLOR` (https://no-color.org) disables ANSI styling.
                let color = std::env::var_os("NO_COLOR").is_none();
                print!("{}", format_project_table(projects, color));
            } else {
                for p in projects {
                    println!("{}", format_project_row(p));
                }
            }
        }
    }
}

/// One-cell summary of a project's relationship to its remote, plus the
/// ANSI colour to render it in. Pure so the status vocabulary can be
/// pinned by unit tests.
///
/// - non-git surfaces (Folder / Idea / Obsidian) → `—` (nothing to sync)
/// - git with no remote configured → `no remote`
/// - ahead / behind the cached upstream → `↑N` / `↓N` (uncommitted tree
///   adds `uncommitted`)
/// - clean and level with upstream → `✓ synced`
fn sync_label(p: &Project) -> (String, &'static str) {
    match p.project_type {
        ProjectType::Folder | ProjectType::Idea | ProjectType::Obsidian => {
            return ("—".to_string(), C_DIM);
        }
        ProjectType::Git | ProjectType::GitHub | ProjectType::GitLab => {}
    }

    let has_remote = p
        .remote_url
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_remote {
        return ("no remote".to_string(), C_YELLOW);
    }

    let mut parts: Vec<String> = Vec::new();
    if p.ahead.unwrap_or(0) > 0 {
        parts.push(format!("↑{}", p.ahead.unwrap_or(0)));
    }
    if p.behind.unwrap_or(0) > 0 {
        parts.push(format!("↓{}", p.behind.unwrap_or(0)));
    }
    if p.git_status.as_deref() == Some("uncommitted") {
        parts.push("uncommitted".to_string());
    }

    if parts.is_empty() {
        ("✓ synced".to_string(), C_GREEN)
    } else {
        (parts.join(" "), C_YELLOW)
    }
}

/// Replace a leading `$HOME` with `~` so the PATH column stays narrow.
fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            if let Some(rest) = path.strip_prefix(home.as_ref()) {
                return format!("~{}", rest);
            }
        }
    }
    path.to_string()
}

/// Left-pad `s` to `width` display columns (counted in `char`s, which is
/// what the std formatter pads by).
fn pad(s: &str, width: usize) -> String {
    format!("{:<width$}", s, width = width)
}

/// Wrap `s` in an ANSI colour when `color` is on, otherwise return it as-is.
fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("{code}{s}{C_RESET}")
    } else {
        s.to_string()
    }
}

/// Render an aligned table of projects: `TYPE  NAME  SYNC  TECH  PATH`.
/// PATH comes last because it's the widest/raggedest column, so its length
/// can't push the others out of alignment. Returns `""` for an empty list
/// (the caller already prints the count to stderr).
fn format_project_table(projects: &[Project], color: bool) -> String {
    if projects.is_empty() {
        return String::new();
    }

    struct Row {
        ptype: String,
        name: String,
        sync: String,
        sync_color: &'static str,
        tech: String,
        path: String,
    }

    let rows: Vec<Row> = projects
        .iter()
        .map(|p| {
            let (sync, sync_color) = sync_label(p);
            Row {
                ptype: project_type_label(&p.project_type).to_string(),
                name: p.name.clone(),
                sync,
                sync_color,
                tech: if p.tech_stack.is_empty() {
                    "—".to_string()
                } else {
                    p.tech_stack.join(", ")
                },
                path: abbreviate_home(&p.path),
            }
        })
        .collect();

    let w = |header: &str, cells: &dyn Fn(&Row) -> &str| -> usize {
        rows.iter()
            .map(|r| cells(r).chars().count())
            .max()
            .unwrap_or(0)
            .max(header.chars().count())
    };
    let w_type = w("TYPE", &|r| r.ptype.as_str());
    let w_name = w("NAME", &|r| r.name.as_str());
    let w_sync = w("SYNC", &|r| r.sync.as_str());
    let w_tech = w("TECH", &|r| r.tech.as_str());

    let mut out = String::new();
    let header = format!(
        "{}  {}  {}  {}  {}",
        pad("TYPE", w_type),
        pad("NAME", w_name),
        pad("SYNC", w_sync),
        pad("TECH", w_tech),
        "PATH",
    );
    out.push_str(&paint(&header, C_BOLD, color));
    out.push('\n');

    for r in &rows {
        let type_color = if matches!(r.ptype.as_str(), "Folder" | "Idea" | "Obsidian") {
            C_DIM
        } else {
            C_CYAN
        };
        out.push_str(&format!(
            "{}  {}  {}  {}  {}\n",
            paint(&pad(&r.ptype, w_type), type_color, color),
            pad(&r.name, w_name),
            paint(&pad(&r.sync, w_sync), r.sync_color, color),
            paint(&pad(&r.tech, w_tech), C_DIM, color),
            paint(&r.path, C_DIM, color),
        ));
    }
    out
}

/// Pretty-printed JSON array of full `Project` records for `--format json`.
/// Same Serde shape as `/api/map` so callers can use the same `jq` filters
/// against either surface. Pretty-print (not compact) so `jq`-less humans
/// can still skim the output; downstream tools that want compact JSON can
/// pipe through `jq -c .`.
fn format_projects_json(projects: &[Project]) -> String {
    // Safe to unwrap: `Project` derives `Serialize` and contains only
    // serializable fields, so `to_string_pretty` cannot fail at runtime.
    serde_json::to_string_pretty(projects).expect("Project is always serializable")
}

/// Default JSON snapshot path for the active list — lives next to the DB
/// file so a project with multiple DBs (per-corpus testing, e.g.) gets a
/// matching pair of files.
fn default_active_export_path(db_path: &Path) -> PathBuf {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join("active-projects.json")
}

/// Build the Hermes-ready JSON payload for the active list. Each row is
/// the raw `active_projects` record (path, activated_at, note) enriched
/// with `name`/`type`/`description`/`tech_stack`/`tags` *when the path
/// is also in the `projects` table*. Unsurveyed-active paths come
/// through with the project metadata fields omitted — Hermes can still
/// `cd` there and the path is the only load-bearing field for context
/// loading.
fn build_active_export(
    active: &[db::ActiveProject],
    project_lookup: &std::collections::HashMap<String, Project>,
) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = active
        .iter()
        .map(|a| {
            let mut entry = serde_json::json!({
                "path": a.path,
                "activated_at": a.activated_at,
            });
            if let Some(note) = &a.note {
                entry["note"] = serde_json::Value::String(note.clone());
            }
            if let Some(p) = project_lookup.get(a.path.trim_end_matches('/')) {
                entry["name"] = serde_json::Value::String(p.name.clone());
                entry["type"] = serde_json::Value::String(
                    match p.project_type {
                        ProjectType::Git => "Git",
                        ProjectType::Folder => "Folder",
                        ProjectType::Idea => "Idea",
                        ProjectType::GitHub => "GitHub",
                        ProjectType::GitLab => "GitLab",
                        ProjectType::Obsidian => "Obsidian",
                    }
                    .to_string(),
                );
                if !p.description.is_empty() {
                    entry["description"] = serde_json::Value::String(p.description.clone());
                }
                if !p.tech_stack.is_empty() {
                    entry["tech_stack"] = serde_json::json!(p.tech_stack);
                }
                if !p.tags.is_empty() {
                    entry["tags"] = serde_json::json!(p.tags);
                }
                if let Some(branch) = &p.git_branch {
                    entry["git_branch"] = serde_json::Value::String(branch.clone());
                }
                if let Some(remote) = &p.remote_url {
                    entry["remote_url"] = serde_json::Value::String(remote.clone());
                }
            }
            entry
        })
        .collect();
    serde_json::json!({
        "generated_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "count": entries.len(),
        "active_projects": entries,
    })
}

/// Write the active-list JSON snapshot. Atomic via the same
/// `<sibling>/.<name>.tmp` + rename dance the project snapshot uses,
/// so a Hermes read can't observe a half-written file even when the
/// CLI is invoked in a tight loop.
fn write_active_export(path: &Path, payload: &serde_json::Value) -> Result<(), String> {
    let json = serde_json::to_string_pretty(payload)
        .map_err(|e| format!("serialize active export: {}", e))?;
    let tmp = project::tmp_path_for(path);
    std::fs::write(&tmp, &json).map_err(|e| format!("write tmp {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} → {}: {}", tmp.display(), path.display(), e)
    })?;
    Ok(())
}

/// Read all projects, snapshot the active set, write the JSON export.
/// Centralised so every mutation path (add/remove/export) goes through
/// the same code — no chance of one path forgetting to refresh the
/// snapshot.
fn refresh_active_export(conn: &rusqlite::Connection, export_path: &Path) -> Result<usize, String> {
    let active = db::list_active(conn)?;
    let projects = db::load_all_projects(conn).unwrap_or_default();
    let lookup: std::collections::HashMap<String, Project> = projects
        .into_iter()
        .map(|p| (p.path.trim_end_matches('/').to_string(), p))
        .collect();
    let payload = build_active_export(&active, &lookup);
    write_active_export(export_path, &payload)?;
    Ok(active.len())
}

/// Tab-separated row for `mercator active list` text output. Columns:
/// activated_at, path, name (or "-"), note (or "-"). Empty fields are
/// rendered as `-` so column counts stay constant for `awk` consumers.
fn format_active_row(
    a: &db::ActiveProject,
    project_lookup: &std::collections::HashMap<String, Project>,
) -> String {
    let name = project_lookup
        .get(a.path.trim_end_matches('/'))
        .map(|p| p.name.as_str())
        .unwrap_or("-");
    let note = a.note.as_deref().unwrap_or("-");
    format!("{}\t{}\t{}\t{}", a.activated_at, a.path, name, note)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Survey {
            mut paths,
            output,
            github,
            github_token,
            gitlab,
            gitlab_token,
            max_repos,
            watch,
            obsidian,
            obsidian_folder,
            obsidian_vault,
            obsidian_sync,
            db: db_path,
        } => {
            // Default to "." when no paths are given
            if paths.is_empty() {
                paths.push(PathBuf::from("."));
            }

            // Open the DB once for the whole survey loop. The DB is the
            // source of truth for the purge blocklist (#24 stage 3b);
            // each iteration reads the blocklist + writes the resulting
            // project set back via `upsert_projects`.
            let mut conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: open db {}: {}", db_path.display(), e);
                    std::process::exit(1);
                }
            };
            // One-shot import of any pre-existing JSON files. After this
            // first call the DB is the source of truth; the import is a
            // no-op if the JSON files don't exist or have already been
            // ingested by an earlier stage-1+ binary.
            let purged_sidecar = db::purged_sidecar_for_map(&output);
            if let Err(e) = db::import_from_json(&mut conn, &output, Some(&purged_sidecar)) {
                eprintln!("  ⚠  initial JSON import failed: {}", e);
            }

            loop {
                let mut all_projects: Vec<Project> = Vec::new();
                let mut local_count = 0;
                for path in &paths {
                    eprintln!("Surveying {}...", path.display());
                    let found = survey_projects(path);
                    eprintln!("  {} found", found.len());
                    local_count += found.len();
                    all_projects.extend(found);
                }

                // Build the remote-source list. Adding a deploy integration
                // (#8) means pushing one more `AnySource::*` variant here.
                let mut remote_sources: Vec<AnySource> = Vec::new();
                if let Some(gh_user) = &github {
                    remote_sources.push(AnySource::GitHub(GitHubSource {
                        username: gh_user.clone(),
                        token: github_token.clone(),
                        max_repos,
                    }));
                }
                if let Some(gl_user) = &gitlab {
                    remote_sources.push(AnySource::GitLab(GitLabSource {
                        username: gl_user.clone(),
                        token: gitlab_token.clone(),
                        max_repos,
                    }));
                }

                // Fetch remote sources concurrently — `join_all` polls all
                // futures together, so the wall-clock time is roughly the
                // slowest source instead of the sum of all of them. Logs
                // are emitted in source order after the await so the
                // output stays deterministic and readable; concurrency
                // would otherwise interleave eprintln from multiple
                // sources and make the line noise unparseable.
                if !remote_sources.is_empty() {
                    for source in &remote_sources {
                        eprintln!("Fetching {}...", source.description());
                    }
                    let results =
                        futures::future::join_all(remote_sources.iter().map(|s| s.fetch())).await;
                    for (source, result) in remote_sources.iter().zip(results) {
                        match result {
                            Ok(repos) => {
                                eprintln!("  fetched {} {} repos", repos.len(), source.name());
                                all_projects.extend(repos);
                            }
                            Err(e) => {
                                eprintln!("  ⚠  {} ({})", e, source.name());
                            }
                        }
                    }
                }

                // Scan Obsidian vault
                if let Some(ref vault_path) = obsidian {
                    if obsidian_sync {
                        eprintln!("Syncing Obsidian vault...");
                        let sync_result = Command::new("ob")
                            .args(["sync", "--path", &vault_path.to_string_lossy()])
                            .output();
                        match sync_result {
                            Ok(o) if o.status.success() => eprintln!("Obsidian sync complete."),
                            Ok(o) => eprintln!(
                                "Warning: ob sync failed: {}",
                                String::from_utf8_lossy(&o.stderr)
                            ),
                            Err(_) => eprintln!("Warning: `ob` command not found. Skipping sync."),
                        }
                    }
                    let vault_name = obsidian_vault
                        .as_deref()
                        .or_else(|| vault_path.file_name().and_then(|n| n.to_str()))
                        .unwrap_or("vault");
                    eprintln!(
                        "Scanning Obsidian vault '{}' at {}...",
                        vault_name,
                        vault_path.display()
                    );
                    let obs_projects =
                        scan_obsidian_vault(vault_path, &obsidian_folder, vault_name);
                    eprintln!("Found {} Obsidian notes/ideas", obs_projects.len());
                    all_projects.extend(obs_projects);
                }

                // Filter out purged paths so they stay gone across re-surveys.
                // DB is the source of truth for the blocklist; on a fresh
                // install the import above hydrated it from the legacy
                // sidecar so the result is the same.
                let purged: std::collections::HashSet<String> = db::list_purged(&conn)
                    .unwrap_or_else(|e| {
                        eprintln!("  ⚠  db blocklist read failed ({}); skipping filter", e);
                        Vec::new()
                    })
                    .into_iter()
                    .collect();
                let before_purge = all_projects.len();
                all_projects.retain(|p| !purged.contains(p.path.trim_end_matches('/')));
                let purged_count = before_purge - all_projects.len();

                let before_dedup = all_projects.len();
                let all_projects = deduplicate_projects(all_projects);
                let mut all_projects = link_obsidian_notes(all_projects);
                let merged = before_dedup - all_projects.len();

                // Auto-tag all projects
                auto_tag_projects(&mut all_projects);

                if let Err(e) = save_map(&all_projects, &output) {
                    eprintln!("Error: {}", e);
                    if watch.is_none() {
                        std::process::exit(1);
                    }
                } else {
                    let github_count = all_projects
                        .iter()
                        .filter(|p| matches!(p.project_type, ProjectType::GitHub))
                        .count();
                    let dirty_count = all_projects
                        .iter()
                        .filter(|p| p.git_status.as_deref() == Some("uncommitted"))
                        .count();

                    println!("[{}] {} projects ({} local, {} github, {} merged, {} purged, {} dirty) -> {}",
                        chrono::Local::now().format("%H:%M:%S"),
                        all_projects.len(), local_count, github_count, merged, purged_count, dirty_count,
                        output.display()
                    );
                }

                // Push this iteration's project set into the DB. The DB
                // is the source of truth (#24 stage 2c+); the JSON file
                // written above is a backup snapshot.
                match db::upsert_projects(&mut conn, &all_projects) {
                    Ok(stats) => eprintln!(
                        "  db: {} new, {} updated -> {}",
                        stats.projects_inserted,
                        stats.projects_updated,
                        db_path.display()
                    ),
                    Err(e) => eprintln!("  ⚠  db upsert failed: {}", e),
                }

                match watch {
                    Some(minutes) => {
                        eprintln!("Next scan in {} min. Press Ctrl+C to stop.", minutes);
                        tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)).await;
                    }
                    None => break,
                }
            }
        }
        Commands::List {
            db: db_path,
            project_type,
            tag,
            tech,
            active,
            no_git,
            no_remote,
            out_of_sync,
            format,
        } => {
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: open db {}: {}", db_path.display(), e);
                    std::process::exit(1);
                }
            };
            let filter = db::ListFilter {
                project_type,
                tag,
                tech,
                active,
                no_git,
                no_remote,
                out_of_sync,
            };
            let projects = match db::list_projects(&conn, &filter) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };
            print_project_list(&projects, format);
            eprintln!("\n{} projects", projects.len());
        }
        Commands::Active {
            action,
            db: db_path,
            export,
        } => {
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: open db {}: {}", db_path.display(), e);
                    std::process::exit(1);
                }
            };
            let export_path = export.unwrap_or_else(|| default_active_export_path(&db_path));

            // Path normalisation: trim trailing slash so `/foo/` and
            // `/foo` collapse to the same DB row. Canonicalisation is
            // intentionally NOT applied — we want activations to work
            // before a path exists on disk (e.g. about to clone).
            let normalize =
                |p: &Path| -> String { p.to_string_lossy().trim_end_matches('/').to_string() };

            match action {
                ActiveCommand::Add { path, note } => {
                    let key = normalize(&path);
                    if key.is_empty() {
                        eprintln!("Error: path is empty");
                        std::process::exit(1);
                    }
                    match db::add_active(&conn, &key, note.as_deref()) {
                        Ok(true) => println!("added {}", key),
                        Ok(false) => println!("updated {}", key),
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            std::process::exit(1);
                        }
                    }
                    match refresh_active_export(&conn, &export_path) {
                        Ok(n) => eprintln!("wrote {} ({} active)", export_path.display(), n),
                        Err(e) => eprintln!("Warning: export failed: {}", e),
                    }
                }
                ActiveCommand::Remove { path } => {
                    let key = normalize(&path);
                    match db::remove_active(&conn, &key) {
                        Ok(true) => println!("removed {}", key),
                        Ok(false) => {
                            eprintln!("Warning: {} was not on the active list", key);
                        }
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            std::process::exit(1);
                        }
                    }
                    match refresh_active_export(&conn, &export_path) {
                        Ok(n) => eprintln!("wrote {} ({} active)", export_path.display(), n),
                        Err(e) => eprintln!("Warning: export failed: {}", e),
                    }
                }
                ActiveCommand::List { format } => {
                    let active = match db::list_active(&conn) {
                        Ok(a) => a,
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            std::process::exit(1);
                        }
                    };
                    let projects = db::load_all_projects(&conn).unwrap_or_default();
                    let lookup: std::collections::HashMap<String, Project> = projects
                        .into_iter()
                        .map(|p| (p.path.trim_end_matches('/').to_string(), p))
                        .collect();
                    match format {
                        OutputFormat::Text => {
                            for a in &active {
                                println!("{}", format_active_row(a, &lookup));
                            }
                        }
                        OutputFormat::Json => {
                            let payload = build_active_export(&active, &lookup);
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&payload)
                                    .expect("active export is always serializable")
                            );
                        }
                    }
                    eprintln!("\n{} active", active.len());
                }
                ActiveCommand::Export => match refresh_active_export(&conn, &export_path) {
                    Ok(n) => println!("wrote {} ({} active)", export_path.display(), n),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                },
            }
        }
        Commands::Search {
            query,
            db: db_path,
            format,
        } => {
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: open db {}: {}", db_path.display(), e);
                    std::process::exit(1);
                }
            };
            let projects = match db::search_projects(&conn, &query) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };
            print_project_list(&projects, format);
            eprintln!("\n{} projects matched {:?}", projects.len(), query);
        }
        Commands::Export {
            out_dir,
            db: db_path,
            obsidian_vault,
            obsidian_folder,
        } => {
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: open db {}: {}", db_path.display(), e);
                    std::process::exit(1);
                }
            };
            let projects = match db::load_all_projects(&conn) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };
            let target = if let Some(vault) = &obsidian_vault {
                vault.join(&obsidian_folder)
            } else {
                out_dir
            };
            eprintln!(
                "Exporting {} projects to {}...",
                projects.len(),
                target.display()
            );
            match run_export(&projects, &target) {
                Ok((written, errors)) => {
                    println!("Wrote {} markdown files to {}", written, target.display());
                    if errors > 0 {
                        eprintln!("⚠  {} files failed to write", errors);
                        std::process::exit(2);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Readme {
            db: db_path,
            inject,
            all,
            project_type,
            tag,
            tech,
            limit,
            title,
            no_badge,
        } => {
            let conn = match db::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Error: open db {}: {}", db_path.display(), e);
                    std::process::exit(1);
                }
            };
            let filter = db::ListFilter {
                project_type,
                tag,
                tech,
                active: !all,
                no_git: false,
                no_remote: false,
                out_of_sync: false,
            };
            let projects = match db::list_projects(&conn, &filter) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };
            let opts = readme::ReadmeOptions {
                title: title.unwrap_or_else(|| "🛠️ What I'm working on".to_string()),
                badge: !no_badge,
                limit,
            };
            let block = readme::render_block(&projects, &opts, chrono::Utc::now());
            match inject {
                Some(file) => {
                    if let Err(e) = readme::inject_file(&file, &block) {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                    let shown = limit.map_or(projects.len(), |n| projects.len().min(n));
                    eprintln!("Updated {} ({} projects)", file.display(), shown);
                }
                None => println!("{}", block),
            }
        }
        Commands::Serve {
            port,
            bind,
            map_file,
            db: db_path,
            refresh,
        } => {
            // Open the DB (creating + applying schema if absent) and import
            // the existing JSON map + purge sidecar so a fresh DB on a
            // long-running install is hydrated immediately. If the DB
            // refuses to open we fall through to a panicking `.expect` —
            // the alternative would be running with no DB and an empty
            // dashboard, which is worse than a clear startup failure.
            let mut conn =
                db::open(&db_path).expect("open db (use --db to point at a writable file)");
            let purged_sidecar = db::purged_sidecar_for_map(&map_file);
            match db::import_from_json(&mut conn, &map_file, Some(&purged_sidecar)) {
                Ok(stats) => eprintln!(
                    "DB ready: {} projects, {} purged, {} active in {} ({} new, {} updated this run)",
                    db::count_projects(&conn).unwrap_or(0),
                    db::count_purged(&conn).unwrap_or(0),
                    db::count_active(&conn).unwrap_or(0),
                    db_path.display(),
                    stats.projects_inserted,
                    stats.projects_updated,
                ),
                Err(e) => eprintln!("Warning: db import failed: {}", e),
            }
            let db_handle = Arc::new(Mutex::new(conn));

            // Load the user-level config (~/.config/mercator/config.toml)
            // for GitHub/GitLab tokens. A missing file is fine — the
            // refresh handler just won't fetch remote sources.
            let cfg = match config::load() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Warning: config load failed ({}); continuing without it", e);
                    config::Config::default()
                }
            };
            let r = cfg.redacted();
            if r.github_user.is_some() || r.gitlab_user.is_some() {
                eprintln!(
                    "Config: github={} (token: {}), gitlab={} (token: {})",
                    r.github_user.as_deref().unwrap_or("-"),
                    if r.github_token_set { "yes" } else { "no" },
                    r.gitlab_user.as_deref().unwrap_or("-"),
                    if r.gitlab_token_set { "yes" } else { "no" },
                );
            }

            let app_state = AppState {
                #[cfg(feature = "swarm")]
                jobs: Arc::new(Mutex::new(Vec::new())),
                #[cfg(feature = "swarm")]
                task_handles: Arc::new(Mutex::new(std::collections::HashMap::new())),
                map_file: map_file.clone(),
                db: db_handle,
                refresh_paths: refresh,
                cfg: Arc::new(Mutex::new(cfg)),
            };

            // /api/* routes are protected by an optional Bearer token
            // (set MERCATOR_TOKEN). Static dist/ files are served without auth
            // since the dashboard HTML itself is public; the API is the
            // sensitive surface.
            let api = Router::new()
                .route("/api/map", get(serve_map_api))
                .route("/api/graph", get(serve_graph_api))
                .route("/api/open-terminal", post(open_terminal))
                .route("/api/git-status", get(get_git_status_api))
                .route("/api/categorize", post(recategorize_api))
                .route("/api/survey/refresh", post(refresh_survey_api))
                .route("/api/skills", get(skills_api))
                .route("/api/settings", get(settings_api).post(settings_update_api))
                .route("/api/project/purge", post(purge_project_api))
                .route("/api/project/restore", post(restore_project_api))
                .route("/api/purged", get(purged_list_api))
                .route("/api/project/tree", get(project_tree_api))
                .route("/api/project/file", get(project_file_api))
                .route("/api/tickets", post(ticket::create_ticket));

            #[cfg(feature = "swarm")]
            let api = api
                .route("/api/agent/run", post(agent::agent_run))
                .route("/api/agent/jobs", get(agent::agent_jobs))
                .route("/api/agent/job/{id}", get(agent::agent_job_detail))
                .route("/api/agent/job/{id}/log", get(agent::agent_job_log))
                .route("/api/agent/job/{id}/cancel", post(agent::agent_cancel));

            let api = api.layer(middleware::from_fn(require_token));

            let app = Router::new()
                .merge(api)
                .with_state(app_state)
                .fallback_service(ServeDir::new("dist"));

            let addr = SocketAddr::from((bind, port));

            // Bind / auth safety check
            let token_set = std::env::var("MERCATOR_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .is_some();
            let is_loopback = bind.is_loopback();
            if !is_loopback && !token_set {
                eprintln!();
                eprintln!("⚠  WARNING: binding to a non-loopback address without MERCATOR_TOKEN.");
                eprintln!("   Anyone reachable on the network can hit /api/* — including");
                eprintln!("   /api/project/file (read any file under a surveyed project) and,");
                eprintln!("   if --features swarm is on, /api/agent/run (spawn paid LLM tasks).");
                eprintln!();
                eprintln!(
                    "   To require auth: MERCATOR_TOKEN=<secret> mercator serve -b {} -p {}",
                    bind, port
                );
                eprintln!("   To stay safe:    mercator serve  (default 127.0.0.1)");
                eprintln!();
            } else if token_set {
                println!("API auth: MERCATOR_TOKEN required (Bearer scheme)");
            }
            println!("Mercator map available at http://{}", addr);
            println!("Press Ctrl+C to stop");

            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .expect("Failed to bind to port");
            axum::serve(listener, app).await.expect("Server error");
        }
    }
}

/// Extract the bearer token from an `Authorization: Bearer <token>` header.
/// Returns `None` if the header is missing, malformed, or uses a different
/// scheme.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

/// Decide whether a request is authorised given the configured expected
/// token (None = no token configured = always allowed) and the request's
/// Authorization header.
fn is_authorised(expected: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    extract_bearer_token(headers) == Some(expected)
}

/// Axum middleware: when `MERCATOR_TOKEN` is set in the environment, every
/// `/api/*` request must carry `Authorization: Bearer <token>`. When the env
/// is unset, requests pass through unchanged.
async fn require_token(
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = std::env::var("MERCATOR_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if is_authorised(expected.as_deref(), &headers) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{
        extract_md_description, render_project_markdown, sanitize_filename, strip_inline_md,
        yaml_escape,
    };
    use crate::skills::name_prefix_group;
    use crate::sources::{
        deduplicate_projects, detect_agent, detect_github_tech_stack, detect_gitlab_tech_stack,
        detect_tech_stack, format_api_error, link_obsidian_notes, normalize_name,
        normalize_remote_url, parse_ahead_behind, parse_link_next, scan_obsidian_vault,
        survey_projects, GitHubRepo, GitLabRepo,
    };
    use crate::tags_graph::domain_keywords;

    // ── strip_inline_md ────────────────────────────────────────────────

    #[test]
    fn strip_inline_md_keeps_link_text_drops_url() {
        assert_eq!(strip_inline_md("see [docs](https://x.com/y)"), "see docs");
    }

    #[test]
    fn strip_inline_md_strips_emphasis_markers() {
        assert_eq!(
            strip_inline_md("**bold** and *italic* and _under_"),
            "bold and italic and under"
        );
    }

    #[test]
    fn strip_inline_md_drops_code_backticks() {
        assert_eq!(strip_inline_md("`x` plain `y`"), "x plain y");
    }

    #[test]
    fn strip_inline_md_passes_plain_text() {
        assert_eq!(strip_inline_md("plain words 123"), "plain words 123");
    }

    #[test]
    fn strip_inline_md_preserves_multibyte_utf8() {
        // Regression: walking bytes-as-chars used to Latin-1-shatter the em-dash
        // (E2 80 94) into "â\u{80}\u{94}". Must survive intact now.
        assert_eq!(
            strip_inline_md("a wiki hub — **ask** it 🚀 café"),
            "a wiki hub — ask it 🚀 café"
        );
    }

    // ── normalize_remote_url ───────────────────────────────────────────

    #[test]
    fn normalize_remote_url_strips_protocol_and_dot_git() {
        assert_eq!(
            normalize_remote_url("https://github.com/zot24/mercator.git"),
            "github.com/zot24/mercator"
        );
    }

    #[test]
    fn normalize_remote_url_handles_ssh_form() {
        assert_eq!(
            normalize_remote_url("git@github.com:zot24/mercator.git"),
            "github.com/zot24/mercator"
        );
    }

    #[test]
    fn normalize_remote_url_lowercases_and_trims_slash() {
        assert_eq!(
            normalize_remote_url("https://GitHub.com/zot24/Mercator/"),
            "github.com/zot24/mercator"
        );
    }

    #[test]
    fn normalize_remote_url_handles_ssh_with_git_user() {
        // Regression: classify-by-remote needs `ssh://git@host/path` to
        // collapse the same as `git@host:path` and `https://host/path`.
        // Pre-fix this returned `git@github.com/zot24/mercator`, which
        // made the host check fail and projects misclassified as Git.
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/zot24/mercator.git"),
            "github.com/zot24/mercator"
        );
    }

    // ── parse_ahead_behind ─────────────────────────────────────────────

    #[test]
    fn parse_ahead_behind_maps_left_right_to_ahead_behind() {
        // `git rev-list --left-right --count @{u}...HEAD` prints
        // "<behind>\t<ahead>" — upstream-only commits first, then
        // local-only. The helper flips that to (ahead, behind).
        assert_eq!(parse_ahead_behind("1\t2"), Some((2, 1)));
    }

    #[test]
    fn parse_ahead_behind_handles_in_sync() {
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
    }

    #[test]
    fn parse_ahead_behind_rejects_empty_and_garbage() {
        // git emits nothing when there's no upstream to compare against.
        assert_eq!(parse_ahead_behind(""), None);
        assert_eq!(parse_ahead_behind("nope"), None);
        assert_eq!(parse_ahead_behind("1"), None);
    }

    // ── sync_label / format_project_table ──────────────────────────────

    fn git_project(name: &str, remote: Option<&str>) -> Project {
        project(name, ProjectType::Git, remote)
    }

    #[test]
    fn sync_label_marks_non_git_as_dash() {
        let p = project("notes", ProjectType::Folder, None);
        assert_eq!(sync_label(&p).0, "—");
    }

    #[test]
    fn sync_label_flags_missing_remote() {
        let p = git_project("local-only", None);
        assert_eq!(sync_label(&p).0, "no remote");
    }

    #[test]
    fn sync_label_shows_ahead_behind_and_dirty() {
        let mut p = git_project("busy", Some("git@github.com:zot24/busy.git"));
        p.ahead = Some(2);
        p.behind = Some(1);
        p.git_status = Some("uncommitted".to_string());
        assert_eq!(sync_label(&p).0, "↑2 ↓1 uncommitted");
    }

    #[test]
    fn sync_label_reports_clean_synced() {
        let mut p = git_project("tidy", Some("git@github.com:zot24/tidy.git"));
        p.ahead = Some(0);
        p.behind = Some(0);
        assert_eq!(sync_label(&p).0, "✓ synced");
    }

    #[test]
    fn format_project_table_aligns_columns_without_color() {
        let projects = vec![
            git_project("a", Some("git@github.com:z/a.git")),
            project("longer-folder-name", ProjectType::Folder, None),
        ];
        let table = format_project_table(&projects, false);
        let lines: Vec<&str> = table.lines().collect();
        // Header + one row per project, no ANSI escapes when color is off.
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("TYPE"));
        assert!(!table.contains('\x1b'));
        // The TYPE column is padded to the header width (4) at minimum, so
        // "Git" is followed by spaces before the gap to NAME.
        assert!(table.contains("Git "));
        assert!(table.contains("longer-folder-name"));
    }

    #[test]
    fn format_project_table_empty_is_blank() {
        assert_eq!(format_project_table(&[], true), "");
    }

    // ── normalize_name ─────────────────────────────────────────────────

    #[test]
    fn normalize_name_strips_separators_and_case() {
        assert_eq!(normalize_name("My-Cool_Project Name"), "mycoolprojectname");
    }

    // ── name_prefix_group ──────────────────────────────────────────────

    #[test]
    fn name_prefix_group_uses_first_hyphen_segment() {
        assert_eq!(name_prefix_group("gsd-debug"), "gsd");
        assert_eq!(name_prefix_group("managing-umami"), "managing");
    }

    #[test]
    fn name_prefix_group_unwraps_plugin_namespace() {
        // Plugin-namespaced form: "<marketplace>:<skill>" — group is the full
        // marketplace name (preserves hyphens), not the first hyphen segment.
        assert_eq!(
            name_prefix_group("zot24-skills:claude-code-expert"),
            "zot24-skills"
        );
        assert_eq!(name_prefix_group("plugin:skill"), "plugin");
    }

    #[test]
    fn name_prefix_group_falls_back_to_core_for_short_or_no_prefix() {
        assert_eq!(name_prefix_group("init"), "core");
        assert_eq!(name_prefix_group("a-b"), "core");
    }

    // ── domain_keywords ────────────────────────────────────────────────

    #[test]
    fn domain_keywords_filters_short_words_and_stopwords() {
        let kws: std::collections::HashSet<String> =
            domain_keywords("the agent and a project for billing")
                .into_iter()
                .collect();
        // "the", "and", "for", "project" → stopwords; "a", "agent" length boundary
        assert!(kws.contains("agent"));
        assert!(kws.contains("billing"));
        assert!(!kws.contains("the"));
        assert!(!kws.contains("and"));
        assert!(!kws.contains("project")); // explicit stopword
    }

    // ── extract_md_description ─────────────────────────────────────────
    // These tests write a temp file under target/ to keep them hermetic.

    fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("mercator-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn extract_md_description_skips_frontmatter_and_headings() {
        let path = write_temp(
            "fm-and-heading.md",
            "---\ntitle: Foo\n---\n\n# Heading\n\nReal description here.\nSecond line continues.\n\nLater paragraph.\n",
        );
        let got = extract_md_description(&path).unwrap();
        assert_eq!(got, "Real description here. Second line continues.");
    }

    #[test]
    fn extract_md_description_uses_blockquote_tagline_as_description() {
        // Common README shape — first prose line is a blockquote tagline.
        // The extractor strips the `>` prefix and treats it as the description.
        let path = write_temp(
            "tagline.md",
            "# Project\n\n> Cartography for your local landscape\n",
        );
        let got = extract_md_description(&path).unwrap();
        assert_eq!(got, "Cartography for your local landscape");
    }

    #[test]
    fn extract_md_description_skips_badges_and_picks_real_paragraph() {
        let path = write_temp(
            "badges.md",
            "# Project\n\n![ci](https://x.com/y.svg)\n[![cov](https://x.com/c.svg)](https://x.com/c)\n\nThe actual description text.\n",
        );
        let got = extract_md_description(&path).unwrap();
        assert_eq!(got, "The actual description text.");
    }

    #[test]
    fn extract_md_description_caps_at_240_chars() {
        let long = "word ".repeat(200);
        let path = write_temp("long.md", &format!("# Title\n\n{}\n", long));
        let got = extract_md_description(&path).unwrap();
        assert!(got.chars().count() <= 241); // 240 + ellipsis
        assert!(got.ends_with('…'));
    }

    #[test]
    fn extract_md_description_returns_none_for_empty() {
        let path = write_temp("empty.md", "# only heading\n\n---\n");
        assert!(extract_md_description(&path).is_none());
    }

    // ── deduplicate_projects ───────────────────────────────────────────

    fn project(name: &str, ptype: ProjectType, remote: Option<&str>) -> Project {
        Project {
            name: name.to_string(),
            path: format!("/tmp/{}", name),
            description: "No description provided.".to_string(),
            project_type: ptype,
            last_modified: None,
            git_branch: None,
            last_commit: None,
            git_status: None,
            ahead: None,
            behind: None,
            tech_stack: vec![],
            remote_url: remote.map(|s| s.to_string()),
            agent_used: None,
            obsidian_url: None,
            obsidian_note_path: None,
            tags: vec![],
        }
    }

    #[test]
    fn deduplicate_merges_local_with_matching_github() {
        // Post-classify-by-remote (#63 follow-up): the local repo's
        // project_type is now GitHub at survey time because its origin
        // points to github.com. Dedup still keeps "local wins" — the
        // merged result is a single GitHub-typed project enriched with
        // the remote's description and tech_stack.
        let local = project(
            "mercator",
            ProjectType::GitHub,
            Some("git@github.com:zot24/mercator.git"),
        );
        let mut remote = project(
            "mercator",
            ProjectType::GitHub,
            Some("https://github.com/zot24/mercator"),
        );
        // Remote-fetched entries set `path` to the html_url; that's how
        // `is_remote_fetched` in dedup discriminates them from local
        // surveys. Without this, the test row looks like a local clone
        // and dedup correctly keeps it as a separate entry.
        remote.path = "https://github.com/zot24/mercator".to_string();
        remote.description = "Cartography for your local landscape".to_string();
        remote.tech_stack = vec!["Rust".to_string()];

        let merged = deduplicate_projects(vec![local, remote]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].project_type, ProjectType::GitHub));
        assert_eq!(
            merged[0].description,
            "Cartography for your local landscape"
        );
        assert!(merged[0].tech_stack.contains(&"Rust".to_string()));
    }

    #[test]
    fn deduplicate_keeps_remote_only_projects() {
        // Match the actual shape `fetch_github_repos` produces: `path`
        // is the html_url, not a filesystem path. The `is_remote_fetched`
        // discriminator in dedup looks for `https://` here.
        let mut only_remote = project("foo", ProjectType::GitHub, Some("https://github.com/x/foo"));
        only_remote.path = "https://github.com/x/foo".to_string();
        let merged = deduplicate_projects(vec![only_remote]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].project_type, ProjectType::GitHub));
    }

    #[test]
    fn deduplicate_keeps_multiple_local_clones_of_the_same_upstream() {
        // Regression: classify-by-remote (#64) made local Git repos
        // with a github.com origin classify as `ProjectType::GitHub`,
        // which routed them through dedup's URL-keyed map. Two clones
        // of the same upstream collided there and the second silently
        // overwrote the first. Locals must always be path-distinct;
        // only API-fetched entries get URL-keyed.
        let mut a = project(
            "mercator",
            ProjectType::GitHub,
            Some("git@github.com:zot24/mercator.git"),
        );
        a.path = "/Users/me/work/mercator".into();
        let mut b = project(
            "mercator-fork",
            ProjectType::GitHub,
            Some("git@github.com:zot24/mercator.git"),
        );
        b.path = "/Users/me/oss/mercator-fork".into();

        let merged = deduplicate_projects(vec![a, b]);
        assert_eq!(merged.len(), 2, "both local clones must survive dedup");
        let paths: std::collections::HashSet<_> = merged.iter().map(|p| p.path.clone()).collect();
        assert!(paths.contains("/Users/me/work/mercator"));
        assert!(paths.contains("/Users/me/oss/mercator-fork"));
    }

    // ── auto_tag_projects ──────────────────────────────────────────────

    #[test]
    fn auto_tag_picks_up_keywords_from_description() {
        let mut p = project("acme", ProjectType::Git, None);
        p.description = "A small CLI tool for Docker workflow automation".to_string();
        let mut v = vec![p];
        auto_tag_projects(&mut v);
        assert!(v[0].tags.contains(&"cli".to_string()));
        assert!(v[0].tags.contains(&"devops".to_string()));
        assert!(v[0].tags.contains(&"automation".to_string()));
    }

    #[test]
    fn auto_tag_returns_empty_when_no_match() {
        let mut p = project("xyzzy", ProjectType::Git, None);
        p.description = "completely unrelated string".to_string();
        let mut v = vec![p];
        auto_tag_projects(&mut v);
        assert!(v[0].tags.is_empty());
    }

    // ── auth helpers ───────────────────────────────────────────────────

    fn hdrs_with_auth(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", value.parse().unwrap());
        h
    }

    #[test]
    fn extract_bearer_token_strips_scheme() {
        assert_eq!(
            extract_bearer_token(&hdrs_with_auth("Bearer secret-123")),
            Some("secret-123")
        );
    }

    #[test]
    fn extract_bearer_token_rejects_other_schemes() {
        assert_eq!(extract_bearer_token(&hdrs_with_auth("Basic abc")), None);
        assert_eq!(extract_bearer_token(&hdrs_with_auth("secret-123")), None);
    }

    #[test]
    fn extract_bearer_token_returns_none_when_header_missing() {
        let h = HeaderMap::new();
        assert_eq!(extract_bearer_token(&h), None);
    }

    #[test]
    fn is_authorised_passes_when_no_token_configured() {
        let h = HeaderMap::new();
        assert!(is_authorised(None, &h));
    }

    #[test]
    fn is_authorised_rejects_missing_token_when_configured() {
        let h = HeaderMap::new();
        assert!(!is_authorised(Some("secret"), &h));
    }

    #[test]
    fn is_authorised_rejects_wrong_token() {
        let h = hdrs_with_auth("Bearer not-the-secret");
        assert!(!is_authorised(Some("secret"), &h));
    }

    #[test]
    fn is_authorised_accepts_correct_bearer_token() {
        let h = hdrs_with_auth("Bearer secret");
        assert!(is_authorised(Some("secret"), &h));
    }

    // ── Source trait + SourceError (#9) ─────────────────────────────────

    #[test]
    fn source_error_display_passes_through_generic() {
        let err = crate::sources::SourceError::Generic(
            "GitHub API error 403: rate limit (rate limit: 0 remaining, resets in 42s) — set a token (issue #2) for authenticated quota".to_string(),
        );
        // Display is the format `eprintln!("  ⚠  {}", e)` would produce.
        // Today's stderr output must be unchanged after the trait swap.
        assert_eq!(
            err.to_string(),
            "GitHub API error 403: rate limit (rate limit: 0 remaining, resets in 42s) — set a token (issue #2) for authenticated quota"
        );
    }

    #[test]
    fn source_error_from_string_lifts_into_generic() {
        let err: crate::sources::SourceError = "boom".to_string().into();
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn github_source_description_distinguishes_auth_state() {
        use crate::sources::{GitHubSource, Source};
        let auth = GitHubSource {
            username: "alice".into(),
            token: Some("ghp_xxx".into()),
            max_repos: None,
        };
        assert_eq!(auth.name(), "GitHub");
        assert_eq!(auth.description(), "GitHub repos for alice (authenticated)");

        let unauth = GitHubSource {
            username: "alice".into(),
            token: None,
            max_repos: None,
        };
        // Unauth GitHub explicitly mentions the 60/hr cap so users know
        // why fetches throttle.
        assert_eq!(
            unauth.description(),
            "GitHub repos for alice (unauthenticated, 60/hr cap)"
        );
    }

    #[test]
    fn gitlab_source_description_distinguishes_auth_state() {
        use crate::sources::{GitLabSource, Source};
        let auth = GitLabSource {
            username: "bob".into(),
            token: Some("glpat-xxx".into()),
            max_repos: None,
        };
        assert_eq!(auth.name(), "GitLab");
        assert_eq!(auth.description(), "GitLab repos for bob (authenticated)");

        let unauth = GitLabSource {
            username: "bob".into(),
            token: None,
            max_repos: None,
        };
        // GitLab unauth doesn't carry the same 60/hr signal as GitHub.
        assert_eq!(
            unauth.description(),
            "GitLab repos for bob (unauthenticated)"
        );
    }

    #[test]
    fn any_source_dispatches_name_and_description() {
        use crate::sources::{AnySource, GitHubSource, GitLabSource};
        let gh = AnySource::GitHub(GitHubSource {
            username: "alice".into(),
            token: None,
            max_repos: None,
        });
        let gl = AnySource::GitLab(GitLabSource {
            username: "bob".into(),
            token: Some("t".into()),
            max_repos: None,
        });
        assert_eq!(gh.name(), "GitHub");
        assert_eq!(gl.name(), "GitLab");
        assert!(gh.description().starts_with("GitHub repos for alice"));
        assert!(gl.description().starts_with("GitLab repos for bob"));
    }

    // ── format_api_error ───────────────────────────────────────────────

    #[test]
    fn format_api_error_extracts_message_from_json_body() {
        let s = format_api_error(
            "GitHub",
            404,
            r#"{"message":"Not Found","documentation_url":"https://docs..."}"#,
            None,
            None,
        );
        assert!(s.starts_with("GitHub API error 404: Not Found"));
    }

    #[test]
    fn format_api_error_falls_back_to_truncated_body() {
        let s = format_api_error("GitLab", 500, "Internal Server Error html...", None, None);
        assert!(s.starts_with("GitLab API error 500: Internal Server Error html..."));
    }

    #[test]
    fn format_api_error_hints_at_token_for_401_403() {
        let s = format_api_error("GitHub", 403, r#"{"message":"rate limit"}"#, None, None);
        assert!(s.contains("set a token"));

        let s = format_api_error("GitHub", 200, "ok", None, None);
        assert!(!s.contains("set a token"));
    }

    #[test]
    fn format_api_error_includes_rate_limit_when_provided() {
        let future_reset = (chrono::Utc::now().timestamp() + 60).to_string();
        let s = format_api_error(
            "GitHub",
            403,
            r#"{"message":"rate limit"}"#,
            Some("0"),
            Some(&future_reset),
        );
        assert!(s.contains("0 remaining"));
        assert!(s.contains("resets in"));
    }

    // ── parse_link_next ────────────────────────────────────────────────

    #[test]
    fn parse_link_next_finds_next_url() {
        // GitHub's actual format
        let h = r#"<https://api.github.com/user/repos?page=2>; rel="next", <https://api.github.com/user/repos?page=20>; rel="last""#;
        assert_eq!(
            parse_link_next(h).as_deref(),
            Some("https://api.github.com/user/repos?page=2")
        );
    }

    #[test]
    fn parse_link_next_returns_none_when_only_last_present() {
        let h = r#"<https://api.github.com/user/repos?page=20>; rel="last""#;
        assert_eq!(parse_link_next(h), None);
    }

    #[test]
    fn parse_link_next_returns_none_for_garbage() {
        assert_eq!(parse_link_next(""), None);
        assert_eq!(parse_link_next("not a link header"), None);
    }

    #[test]
    fn parse_link_next_handles_first_and_prev_too() {
        // Real header on a middle page has prev, first, next, last
        let h = r#"<https://api.github.com/user/repos?page=1>; rel="first", <https://api.github.com/user/repos?page=2>; rel="prev", <https://api.github.com/user/repos?page=4>; rel="next", <https://api.github.com/user/repos?page=20>; rel="last""#;
        assert_eq!(
            parse_link_next(h).as_deref(),
            Some("https://api.github.com/user/repos?page=4")
        );
    }

    // ── parse_git_status_path ──────────────────────────────────────────

    #[test]
    fn parse_git_status_path_handles_modified() {
        assert_eq!(parse_git_status_path(" M src/main.rs"), Some("src/main.rs"));
        assert_eq!(parse_git_status_path("M  Cargo.toml"), Some("Cargo.toml"));
    }

    #[test]
    fn parse_git_status_path_handles_added_and_untracked() {
        assert_eq!(parse_git_status_path("A  new.txt"), Some("new.txt"));
        assert_eq!(
            parse_git_status_path("?? untracked.md"),
            Some("untracked.md")
        );
    }

    #[test]
    fn parse_git_status_path_unwraps_rename() {
        assert_eq!(parse_git_status_path("R  old.rs -> new.rs"), Some("new.rs"));
    }

    #[test]
    fn parse_git_status_path_returns_none_for_empty() {
        assert_eq!(parse_git_status_path(""), None);
        assert_eq!(parse_git_status_path("  "), None);
    }

    // ── sanitize_filename ──────────────────────────────────────────────

    #[test]
    fn sanitize_filename_replaces_filesystem_hostile_chars() {
        assert_eq!(sanitize_filename("my/repo:name"), "my-repo-name");
        assert_eq!(sanitize_filename("a*b?c|d"), "a-b-c-d");
    }

    #[test]
    fn sanitize_filename_collapses_repeated_dashes() {
        assert_eq!(sanitize_filename("a//b"), "a-b");
        assert_eq!(sanitize_filename("x///y"), "x-y");
    }

    #[test]
    fn sanitize_filename_falls_back_to_untitled_for_empty() {
        assert_eq!(sanitize_filename(""), "untitled");
        assert_eq!(sanitize_filename("...   "), "untitled");
    }

    #[test]
    fn sanitize_filename_preserves_normal_names() {
        assert_eq!(sanitize_filename("mercator"), "mercator");
        assert_eq!(sanitize_filename("My Cool Project"), "My Cool Project");
    }

    // ── yaml_escape ────────────────────────────────────────────────────

    #[test]
    fn yaml_escape_quotes_when_needed() {
        assert_eq!(yaml_escape("simple"), "simple");
        assert_eq!(yaml_escape("has: colon"), "\"has: colon\"");
        assert_eq!(yaml_escape("- starts dash"), "\"- starts dash\"");
        assert_eq!(yaml_escape(""), "\"\"");
    }

    #[test]
    fn yaml_escape_handles_quotes_in_value() {
        assert_eq!(yaml_escape("a\"b"), "\"a\\\"b\"");
    }

    // ── render_project_markdown ────────────────────────────────────────

    #[test]
    fn render_project_markdown_includes_frontmatter_and_heading() {
        let mut p = project(
            "mercator",
            ProjectType::Git,
            Some("https://github.com/zot24/mercator"),
        );
        p.description = "Cartography for your local landscape".to_string();
        p.git_branch = Some("master".to_string());
        p.tech_stack = vec!["Rust".to_string()];
        p.tags = vec!["cli".to_string(), "docs".to_string()];

        let md = render_project_markdown(&p);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("name: mercator"));
        assert!(md.contains("type: Git"));
        assert!(md.contains("branch: master"));
        assert!(md.contains("# mercator"));
        assert!(md.contains("Cartography for your local landscape"));
        assert!(md.contains("- **Branch**: `master`"));
        assert!(md.contains("[Remote](https://github.com/zot24/mercator)"));
        assert!(md.contains("Tags: #cli #docs"));
        assert!(md.contains("Stack: Rust"));
    }

    #[test]
    fn render_project_markdown_omits_empty_sections() {
        let p = project("bare", ProjectType::Folder, None);
        let md = render_project_markdown(&p);
        assert!(!md.contains("## Status"));
        assert!(!md.contains("## Links"));
        assert!(md.contains("# bare"));
    }

    // ── detect_github_tech_stack ───────────────────────────────────────

    fn gh_repo(language: Option<&str>, topics: Option<Vec<&str>>) -> GitHubRepo {
        GitHubRepo {
            name: "demo".into(),
            description: None,
            html_url: "https://github.com/x/demo".into(),
            pushed_at: "2026-05-04T00:00:00Z".into(),
            default_branch: None,
            language: language.map(str::to_string),
            topics: topics.map(|v| v.into_iter().map(str::to_string).collect()),
        }
    }

    #[test]
    fn detect_github_tech_stack_puts_language_first() {
        let r = gh_repo(Some("Rust"), Some(vec!["cli", "tooling"]));
        assert_eq!(detect_github_tech_stack(&r), vec!["Rust", "cli", "tooling"]);
    }

    #[test]
    fn detect_github_tech_stack_caps_topics_at_three() {
        let r = gh_repo(Some("Go"), Some(vec!["a", "b", "c", "d", "e"]));
        // language + first three topics only
        assert_eq!(detect_github_tech_stack(&r), vec!["Go", "a", "b", "c"]);
    }

    #[test]
    fn detect_github_tech_stack_dedups_topic_against_language() {
        let r = gh_repo(Some("rust"), Some(vec!["rust", "cli"]));
        // language "rust" preserved; identical topic skipped
        assert_eq!(detect_github_tech_stack(&r), vec!["rust", "cli"]);
    }

    #[test]
    fn detect_github_tech_stack_handles_missing_fields() {
        assert!(detect_github_tech_stack(&gh_repo(None, None)).is_empty());
        assert_eq!(
            detect_github_tech_stack(&gh_repo(None, Some(vec!["docker"]))),
            vec!["docker"]
        );
        assert_eq!(
            detect_github_tech_stack(&gh_repo(Some("Python"), None)),
            vec!["Python"]
        );
    }

    // ── detect_gitlab_tech_stack ───────────────────────────────────────

    fn gl_repo(tags: Option<Vec<&str>>) -> GitLabRepo {
        GitLabRepo {
            name: "demo".into(),
            description: None,
            web_url: "https://gitlab.com/x/demo".into(),
            last_activity_at: "2026-05-04T00:00:00Z".into(),
            default_branch: None,
            tag_list: tags.map(|v| v.into_iter().map(str::to_string).collect()),
        }
    }

    #[test]
    fn detect_gitlab_tech_stack_caps_tags_at_three() {
        let r = gl_repo(Some(vec!["a", "b", "c", "d"]));
        assert_eq!(detect_gitlab_tech_stack(&r), vec!["a", "b", "c"]);
    }

    #[test]
    fn detect_gitlab_tech_stack_handles_missing_or_empty() {
        assert!(detect_gitlab_tech_stack(&gl_repo(None)).is_empty());
        assert!(detect_gitlab_tech_stack(&gl_repo(Some(vec![]))).is_empty());
    }

    // ── link_obsidian_notes ────────────────────────────────────────────

    fn obsidian_project(name: &str, url: &str, note_path: &str) -> Project {
        let mut p = project(name, ProjectType::Obsidian, None);
        p.obsidian_url = Some(url.to_string());
        p.obsidian_note_path = Some(note_path.to_string());
        p
    }

    #[test]
    fn link_obsidian_notes_merges_url_into_matching_project() {
        let local = project("My-Cool_Project", ProjectType::Git, None);
        let obs = obsidian_project(
            "my cool project",
            "obsidian://open?vault=v&file=Projects/note",
            "Projects/note",
        );

        let merged = link_obsidian_notes(vec![local, obs]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].project_type, ProjectType::Git));
        assert_eq!(
            merged[0].obsidian_url.as_deref(),
            Some("obsidian://open?vault=v&file=Projects/note")
        );
        assert_eq!(
            merged[0].obsidian_note_path.as_deref(),
            Some("Projects/note")
        );
    }

    #[test]
    fn link_obsidian_notes_keeps_unmatched_obsidian_standalone() {
        let local = project("alpha", ProjectType::Git, None);
        let obs = obsidian_project("beta", "obsidian://x", "Projects/beta");
        let merged = link_obsidian_notes(vec![local, obs]);
        assert_eq!(merged.len(), 2);
        let alpha = merged.iter().find(|p| p.name == "alpha").unwrap();
        assert!(alpha.obsidian_url.is_none());
        let beta = merged.iter().find(|p| p.name == "beta").unwrap();
        assert!(matches!(beta.project_type, ProjectType::Obsidian));
    }

    #[test]
    fn link_obsidian_notes_first_obsidian_wins_on_collision() {
        // Two obsidian notes normalize to the same key — second overwrites first
        // in the index. Pin this behavior so a future map → fold rewrite doesn't
        // change which one survives.
        let local = project("foo", ProjectType::Git, None);
        let obs1 = obsidian_project("foo", "obsidian://first", "Projects/first");
        let obs2 = obsidian_project("FOO", "obsidian://second", "Projects/second");
        let merged = link_obsidian_notes(vec![local, obs1, obs2]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].obsidian_url.as_deref(), Some("obsidian://second"));
    }

    // ── detect_tech_stack (FS) ─────────────────────────────────────────

    #[test]
    fn detect_tech_stack_picks_up_cargo_and_package_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();

        let stack = detect_tech_stack(dir.path());
        assert!(stack.contains(&"Rust".to_string()));
        assert!(stack.contains(&"Node.js".to_string()));
    }

    #[test]
    fn detect_tech_stack_dedups_same_tech_across_markers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "").unwrap();

        let stack = detect_tech_stack(dir.path());
        assert_eq!(stack.iter().filter(|t| *t == "Rust").count(), 1);
    }

    #[test]
    fn detect_tech_stack_returns_empty_for_unknown_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# nothing").unwrap();
        assert!(detect_tech_stack(dir.path()).is_empty());
    }

    // ── detect_agent (FS) ──────────────────────────────────────────────

    #[test]
    fn detect_agent_finds_claude_via_md_or_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# project").unwrap();
        assert_eq!(detect_agent(dir.path()).as_deref(), Some("claude"));

        let dir2 = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir2.path().join(".claude")).unwrap();
        assert_eq!(detect_agent(dir2.path()).as_deref(), Some("claude"));
    }

    #[test]
    fn detect_agent_finds_codex_when_no_claude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();
        assert_eq!(detect_agent(dir.path()).as_deref(), Some("codex"));
    }

    #[test]
    fn detect_agent_prefers_claude_when_both_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();
        assert_eq!(detect_agent(dir.path()).as_deref(), Some("claude"));
    }

    #[test]
    fn detect_agent_returns_none_for_plain_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_agent(dir.path()).is_none());
    }

    // ── classify_git_project_type ──────────────────────────────────────

    #[test]
    fn classify_git_project_type_handles_no_remote() {
        use crate::sources::classify_git_project_type;
        assert!(matches!(classify_git_project_type(None), ProjectType::Git));
        assert!(
            matches!(classify_git_project_type(Some("")), ProjectType::Git),
            "empty string is not a remote"
        );
    }

    #[test]
    fn classify_git_project_type_recognizes_github() {
        use crate::sources::classify_git_project_type;
        for url in [
            "https://github.com/zot24/mercator",
            "https://github.com/zot24/mercator.git",
            "git@github.com:zot24/mercator.git",
            "ssh://git@github.com/zot24/mercator.git",
            "https://GitHub.com/zot24/mercator", // case-insensitive
        ] {
            assert!(
                matches!(classify_git_project_type(Some(url)), ProjectType::GitHub),
                "expected GitHub for {url}"
            );
        }
    }

    #[test]
    fn classify_git_project_type_recognizes_gitlab() {
        use crate::sources::classify_git_project_type;
        for url in [
            "https://gitlab.com/user/repo",
            "git@gitlab.com:user/repo.git",
            "https://gitlab.example.com/user/repo", // self-hosted
            "https://gitlab.internal.work/team/repo",
        ] {
            assert!(
                matches!(classify_git_project_type(Some(url)), ProjectType::GitLab),
                "expected GitLab for {url}"
            );
        }
    }

    #[test]
    fn classify_git_project_type_falls_back_to_git_for_other_hosts() {
        use crate::sources::classify_git_project_type;
        for url in [
            "https://bitbucket.org/x/y",
            "https://codeberg.org/x/y",
            "git@gitea.example.com:x/y.git",
            "https://git.sr.ht/~user/repo",
            // A URL whose path mentions "gitlab" but whose host doesn't —
            // false-positive guard.
            "https://github.com/x/my-gitlab-mirror",
        ] {
            let got = classify_git_project_type(Some(url));
            // The github.com one is still GitHub; the others are Git.
            if url.contains("github.com") {
                assert!(
                    matches!(got, ProjectType::GitHub),
                    "expected GitHub for {url}"
                );
            } else {
                assert!(
                    matches!(got, ProjectType::Git),
                    "expected Git for {url}, got {:?}",
                    got
                );
            }
        }
    }

    // ── survey_projects (FS) ───────────────────────────────────────────

    #[test]
    fn survey_projects_classifies_git_idea_and_folder() {
        let root = tempfile::tempdir().unwrap();

        // Git project: has .git
        let git_proj = root.path().join("alpha");
        std::fs::create_dir(&git_proj).unwrap();
        std::fs::create_dir(git_proj.join(".git")).unwrap();

        // Idea project: has IDEA.md but no .git
        let idea_proj = root.path().join("beta");
        std::fs::create_dir(&idea_proj).unwrap();
        std::fs::write(idea_proj.join("IDEA.md"), "# idea").unwrap();

        // Plain folder: no .git, no IDEA.md
        let folder = root.path().join("gamma");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("notes.txt"), "").unwrap();

        let projects = survey_projects(root.path());
        let by_name: std::collections::HashMap<_, _> =
            projects.iter().map(|p| (p.name.clone(), p)).collect();

        assert!(matches!(by_name["alpha"].project_type, ProjectType::Git));
        assert!(matches!(by_name["beta"].project_type, ProjectType::Idea));
        assert!(matches!(by_name["gamma"].project_type, ProjectType::Folder));
    }

    #[test]
    fn survey_projects_skips_target_and_node_modules_siblings() {
        // Children named target/node_modules should be skipped, even when they
        // would otherwise classify as plain Folders. Sibling `keep` is the
        // positive control proving the survey is actually walking the root.
        // (`.git` siblings are intentionally not tested here — adding `.git`
        // as a child of root makes root itself a Git repo, which is the
        // correct survey behavior. Skip-of-`.git` is exercised by
        // `survey_projects_does_not_descend_into_git_repo`.)
        let root = tempfile::tempdir().unwrap();
        for skip in &["target", "node_modules"] {
            std::fs::create_dir(root.path().join(skip)).unwrap();
        }
        std::fs::create_dir(root.path().join("keep")).unwrap();

        let names: std::collections::HashSet<String> = survey_projects(root.path())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert!(names.contains("keep"), "got {:?}", names);
        assert!(!names.contains("target"));
        assert!(!names.contains("node_modules"));
    }

    #[test]
    fn survey_projects_does_not_descend_into_git_repo() {
        // A Git repo containing a sub-dir with its own .git should NOT yield the
        // nested project — survey calls skip_current_dir on classification.
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("outer");
        std::fs::create_dir(&outer).unwrap();
        std::fs::create_dir(outer.join(".git")).unwrap();
        let inner = outer.join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::create_dir(inner.join(".git")).unwrap();

        let projects = survey_projects(root.path());
        let names: Vec<_> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["outer"]);
    }

    // ── scan_obsidian_vault (FS) ───────────────────────────────────────

    #[test]
    fn scan_obsidian_vault_classifies_subfolders_md_and_at_projects() {
        let vault = tempfile::tempdir().unwrap();
        let projects_dir = vault.path().join("Projects");
        std::fs::create_dir(&projects_dir).unwrap();

        // Subfolder project with a single .md note inside
        let sub = projects_dir.join("alpha");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("alpha.md"), "Some description.\n").unwrap();

        // Standalone .md note
        std::fs::write(projects_dir.join("beta.md"), "Beta note body.\n").unwrap();

        // Bullet list of ideas
        std::fs::write(
            projects_dir.join("@Projects.md"),
            "# Ideas\n\n- gamma idea\n- delta idea\n\nNot a bullet.\n",
        )
        .unwrap();

        let found = scan_obsidian_vault(vault.path(), "Projects", "myvault");
        let names: std::collections::HashSet<_> = found.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        assert!(names.contains("gamma idea"));
        assert!(names.contains("delta idea"));
        // All entries are typed Obsidian
        assert!(found
            .iter()
            .all(|p| matches!(p.project_type, ProjectType::Obsidian)));
        // Each has an obsidian_url scheme
        assert!(found.iter().all(|p| p
            .obsidian_url
            .as_deref()
            .unwrap()
            .starts_with("obsidian://")));
    }

    #[test]
    fn scan_obsidian_vault_returns_empty_when_folder_missing() {
        let vault = tempfile::tempdir().unwrap();
        let found = scan_obsidian_vault(vault.path(), "Projects", "myvault");
        assert!(found.is_empty());
    }

    #[test]
    fn scan_obsidian_vault_skips_dotfiles() {
        let vault = tempfile::tempdir().unwrap();
        let projects_dir = vault.path().join("Projects");
        std::fs::create_dir(&projects_dir).unwrap();
        std::fs::write(projects_dir.join(".obsidian.md"), "should be skipped").unwrap();
        std::fs::write(projects_dir.join("real.md"), "real").unwrap();

        let found = scan_obsidian_vault(vault.path(), "Projects", "myvault");
        let names: Vec<_> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    // ── list / search output formats ───────────────────────────────────

    #[test]
    fn format_project_row_emits_five_tab_separated_columns() {
        // Stable contract for shell pipelines (`awk -F'\t' '{print $2}'`).
        // The columns are: type, path, name, tags, tech.
        let mut p = project("acme", ProjectType::Git, None);
        p.path = "/tmp/acme".into();
        p.tags = vec!["cli".into(), "devops".into()];
        p.tech_stack = vec!["Rust".into(), "Tokio".into()];

        let row = format_project_row(&p);
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(
            cols,
            vec!["Git", "/tmp/acme", "acme", "cli,devops", "Rust,Tokio"]
        );
    }

    #[test]
    fn format_project_row_handles_empty_tags_and_tech() {
        // Empty vec must serialize as an empty column, not the literal
        // `[]` — pipelines split on `\t` and expect to see five fields.
        let p = project("bare", ProjectType::Folder, None);
        let row = format_project_row(&p);
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(cols.len(), 5);
        assert_eq!(cols[3], "");
        assert_eq!(cols[4], "");
    }

    #[test]
    fn format_projects_json_round_trips_through_project_struct() {
        // The JSON output must deserialize back into `Vec<Project>`
        // unchanged — this is the wire-shape contract for downstream
        // tooling and matches the `/api/map` payload.
        let mut p = project(
            "acme",
            ProjectType::GitHub,
            Some("https://github.com/x/acme"),
        );
        p.tags = vec!["cli".into()];
        p.tech_stack = vec!["Rust".into()];
        p.description = "A test project".into();
        let input = vec![p];

        let json = format_projects_json(&input);
        let parsed: Vec<Project> = serde_json::from_str(&json).expect("output must be valid JSON");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "acme");
        assert!(matches!(parsed[0].project_type, ProjectType::GitHub));
        assert_eq!(parsed[0].tags, vec!["cli".to_string()]);
        assert_eq!(parsed[0].tech_stack, vec!["Rust".to_string()]);
        assert_eq!(
            parsed[0].remote_url.as_deref(),
            Some("https://github.com/x/acme")
        );
    }

    #[test]
    fn format_projects_json_empty_input_is_empty_array() {
        // `mercator list --format json` on a no-match filter must emit a
        // valid empty JSON array — `jq` consumers rely on this rather
        // than having to special-case empty stdout.
        let out = format_projects_json(&[]);
        assert_eq!(out.trim(), "[]");
    }

    #[test]
    fn format_projects_json_uses_camelcase_field_names() {
        // The Serde renames on `Project` (`techStack`, `remoteUrl`, …)
        // are part of the public contract — the JSON shape must match
        // what `/api/map` already serves so downstream `jq` filters work
        // against either surface unchanged.
        let mut p = project("acme", ProjectType::Git, Some("https://example.com/acme"));
        p.tech_stack = vec!["Rust".into()];
        p.git_branch = Some("master".into());

        let json = format_projects_json(&[p]);
        assert!(json.contains("\"techStack\""));
        assert!(json.contains("\"remoteUrl\""));
        assert!(json.contains("\"gitBranch\""));
        assert!(!json.contains("\"tech_stack\""));
        assert!(!json.contains("\"remote_url\""));
    }

    // ── active export helpers ──────────────────────────────────────────

    fn sample_project_for_active(name: &str, path: &str) -> Project {
        Project {
            name: name.into(),
            path: path.into(),
            description: format!("{} description", name),
            project_type: ProjectType::Git,
            last_modified: None,
            git_branch: Some("master".into()),
            last_commit: None,
            git_status: None,
            ahead: None,
            behind: None,
            tech_stack: vec!["Rust".into()],
            remote_url: Some("https://example.test/repo".into()),
            agent_used: None,
            obsidian_url: None,
            obsidian_note_path: None,
            tags: vec!["cli".into()],
        }
    }

    #[test]
    fn default_active_export_path_sits_next_to_db() {
        assert_eq!(
            default_active_export_path(Path::new("/tmp/x/mercator.db")),
            PathBuf::from("/tmp/x/active-projects.json"),
        );
        assert_eq!(
            default_active_export_path(Path::new("mercator.db")),
            PathBuf::from("active-projects.json"),
        );
    }

    #[test]
    fn build_active_export_enriches_surveyed_paths_only() {
        // alpha is surveyed → enriched. ghost is active-only → bare.
        let active = vec![
            db::ActiveProject {
                path: "/tmp/alpha".into(),
                activated_at: "2026-05-01T00:00:00.000Z".into(),
                note: Some("planning v2".into()),
            },
            db::ActiveProject {
                path: "/tmp/ghost".into(),
                activated_at: "2026-05-02T00:00:00.000Z".into(),
                note: None,
            },
        ];
        let mut lookup = std::collections::HashMap::new();
        lookup.insert(
            "/tmp/alpha".to_string(),
            sample_project_for_active("alpha", "/tmp/alpha"),
        );

        let payload = build_active_export(&active, &lookup);
        assert_eq!(payload["count"], 2);
        let entries = payload["active_projects"].as_array().unwrap();
        assert_eq!(entries.len(), 2);

        // First entry: alpha — enriched with project metadata.
        assert_eq!(entries[0]["path"], "/tmp/alpha");
        assert_eq!(entries[0]["name"], "alpha");
        assert_eq!(entries[0]["type"], "Git");
        assert_eq!(entries[0]["description"], "alpha description");
        assert_eq!(entries[0]["note"], "planning v2");
        assert_eq!(entries[0]["tech_stack"][0], "Rust");
        assert_eq!(entries[0]["tags"][0], "cli");
        assert_eq!(entries[0]["git_branch"], "master");

        // Second entry: ghost — only the load-bearing fields, no enrichment
        // and no note (Option::None is dropped from the JSON shape).
        assert_eq!(entries[1]["path"], "/tmp/ghost");
        assert!(entries[1].get("name").is_none());
        assert!(entries[1].get("note").is_none());
        assert!(entries[1].get("description").is_none());
    }

    #[test]
    fn build_active_export_trims_trailing_slash_when_joining_lookup() {
        // Regression guard: active rows are stored without a trailing
        // slash, but the projects table may have either form. Trim
        // both sides at lookup so enrichment survives the mismatch.
        let active = vec![db::ActiveProject {
            path: "/tmp/alpha".into(),
            activated_at: "2026-05-01T00:00:00.000Z".into(),
            note: None,
        }];
        let mut lookup = std::collections::HashMap::new();
        let mut p = sample_project_for_active("alpha", "/tmp/alpha/");
        p.path = "/tmp/alpha/".into();
        lookup.insert("/tmp/alpha".to_string(), p);

        let payload = build_active_export(&active, &lookup);
        let entries = payload["active_projects"].as_array().unwrap();
        assert_eq!(entries[0]["name"], "alpha");
    }

    #[test]
    fn format_active_row_renders_dashes_for_missing_fields() {
        let a = db::ActiveProject {
            path: "/tmp/ghost".into(),
            activated_at: "2026-05-01T00:00:00.000Z".into(),
            note: None,
        };
        let lookup = std::collections::HashMap::new();
        let row = format_active_row(&a, &lookup);
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0], "2026-05-01T00:00:00.000Z");
        assert_eq!(cols[1], "/tmp/ghost");
        assert_eq!(cols[2], "-");
        assert_eq!(cols[3], "-");
    }

    #[test]
    fn write_active_export_writes_atomically() {
        // Sibling .tmp must not survive the rename — same invariant as
        // save_map. Pure filesystem check.
        let dir = std::env::temp_dir().join(format!("mercator-active-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("active-projects.json");
        let payload = serde_json::json!({"active_projects": [], "count": 0});
        write_active_export(&target, &payload).unwrap();
        assert!(target.exists());
        assert!(!project::tmp_path_for(&target).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
