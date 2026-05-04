// Mercator - Project Topography Tool
// A Rust CLI tool for discovering and visualizing your local development projects

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(feature = "swarm")]
use std::sync::Arc;
use std::time::SystemTime;
#[cfg(feature = "swarm")]
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(name = "mercator")]
#[command(about = "Cartography for your local projects", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
    },
    /// Export the map as one markdown file per project (one folder of
    /// structured notes that any other tool can consume)
    Export {
        /// Output directory (created if missing). Defaults to `./mercator-export`.
        #[arg(default_value = "mercator-export")]
        out_dir: PathBuf,

        /// Source map JSON to read from
        #[arg(short, long, default_value = "mercator_map.json")]
        map_file: PathBuf,

        /// When set, write under `<vault>/<folder>/` instead of `out_dir`.
        /// Feed the Obsidian LLM-wiki layer (issue #22).
        #[arg(long)]
        obsidian_vault: Option<PathBuf>,

        /// Subdirectory inside the Obsidian vault (default: "Projects")
        #[arg(long, default_value = "Projects")]
        obsidian_folder: String,
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
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Project {
    name: String,
    path: String,
    description: String,
    project_type: ProjectType,
    #[serde(rename = "lastModified")]
    last_modified: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    #[serde(rename = "lastCommit")]
    last_commit: Option<String>,
    #[serde(rename = "gitStatus")]
    git_status: Option<String>,
    #[serde(rename = "techStack")]
    tech_stack: Vec<String>,
    #[serde(rename = "remoteUrl")]
    remote_url: Option<String>,
    /// Detected AI agent used in this project (e.g., "claude", "codex")
    #[serde(rename = "agentUsed")]
    agent_used: Option<String>,
    /// Obsidian URI to open the linked note
    #[serde(
        rename = "obsidianUrl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    obsidian_url: Option<String>,
    /// Relative path to the Obsidian note within the vault
    #[serde(
        rename = "obsidianNotePath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    obsidian_note_path: Option<String>,
    /// Auto-generated topic tags for graph edges and semantic grouping
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum ProjectType {
    Git,
    Folder,
    Idea,
    GitHub,
    GitLab,
    Obsidian,
}

/// Formats a SystemTime as an ISO 8601 string
fn format_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Run a git command and return the output (trimmed)
fn git_command(path: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
}

/// Get git status for a repository
fn get_git_info(
    path: &Path,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let branch = git_command(path, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let commit = git_command(path, &["log", "-1", "--pretty=%s"]);
    let remote_url = git_command(path, &["remote", "get-url", "origin"]);

    let status = git_command(path, &["status", "--porcelain"]);
    let git_status = if status.as_ref().map(|s| !s.is_empty()).unwrap_or(false) {
        Some("uncommitted".to_string())
    } else {
        None
    };

    (branch, commit, git_status, remote_url)
}

/// Format an HTTP error response into a single-line summary suitable for
/// stderr. Pulls the JSON `message` field if present (GitHub / GitLab both
/// use it for 4xx errors) and includes any rate-limit headers GitHub
/// returns. Pure function so it can be unit-tested without HTTP.
fn format_api_error(
    provider: &str,
    status: u16,
    body: &str,
    rate_remaining: Option<&str>,
    rate_reset_epoch: Option<&str>,
) -> String {
    let parsed_message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        });
    let message = parsed_message.unwrap_or_else(|| {
        // Fallback: short truncated body
        body.chars().take(200).collect::<String>()
    });
    let mut out = format!("{} API error {}: {}", provider, status, message);
    if let (Some(rem), Some(reset)) = (rate_remaining, rate_reset_epoch) {
        // GitHub-style rate limit hint
        if let Ok(reset_n) = reset.parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            let secs = (reset_n - now).max(0);
            out.push_str(&format!(
                " (rate limit: {} remaining, resets in {}s)",
                rem, secs
            ));
        }
    }
    if status == 401 || status == 403 {
        out.push_str(" — set a token (issue #2) for authenticated quota");
    }
    out
}

/// Parse a GitHub `Link` header and return the URL with `rel="next"` if any.
/// Pure function so it can be unit-tested without HTTP. The header looks like
/// `<https://...?page=2>; rel="next", <https://...?page=20>; rel="last"`.
fn parse_link_next(link_header: &str) -> Option<String> {
    for part in link_header.split(',') {
        let part = part.trim();
        // Find the angle-bracketed URL and the rel attribute
        let url = part.strip_prefix('<').and_then(|s| s.split_once('>'))?;
        let (link_url, attrs) = url;
        if attrs.contains("rel=\"next\"") {
            return Some(link_url.to_string());
        }
    }
    None
}

/// Fetch repositories from GitHub API. Paginates via `Link: rel="next"` and
/// authenticates with a token if provided.
async fn fetch_github_repos(
    username: &str,
    token: Option<&str>,
    max_repos: Option<usize>,
) -> Result<Vec<Project>, String> {
    let client = reqwest::Client::new();
    let mut next_url = Some(format!(
        "https://api.github.com/users/{}/repos?per_page=100&sort=pushed",
        username
    ));
    let mut all_repos: Vec<GitHubRepo> = Vec::new();

    while let Some(url) = next_url.take() {
        let mut req = client
            .get(&url)
            .header("User-Agent", "Mercator/1.0")
            .header("Accept", "application/vnd.github.v3+json");
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        let response = req
            .send()
            .await
            .map_err(|e| format!("GitHub request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let rate_remaining = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let rate_reset = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = response.text().await.unwrap_or_default();
            return Err(format_api_error(
                "GitHub",
                status,
                &body,
                rate_remaining.as_deref(),
                rate_reset.as_deref(),
            ));
        }

        // Capture next-page link before consuming the response body
        let link_header = response
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let mut page: Vec<GitHubRepo> = response
            .json()
            .await
            .map_err(|e| format!("GitHub response was not a repo array: {}", e))?;

        all_repos.append(&mut page);

        if max_repos.is_some_and(|m| all_repos.len() >= m) {
            break;
        }
        next_url = link_header.as_deref().and_then(parse_link_next);
    }

    let take = max_repos.unwrap_or(usize::MAX);
    let projects = all_repos
        .into_iter()
        .take(take)
        .map(|repo| {
            let tech_stack = detect_github_tech_stack(&repo);
            Project {
                name: repo.name,
                path: repo.html_url.clone(),
                description: repo.description.unwrap_or_default(),
                project_type: ProjectType::GitHub,
                last_modified: Some(repo.pushed_at),
                git_branch: Some(repo.default_branch.unwrap_or_else(|| "main".to_string())),
                last_commit: None,
                git_status: None,
                tech_stack,
                remote_url: Some(repo.html_url),
                agent_used: None,
                obsidian_url: None,
                obsidian_note_path: None,
                tags: vec![],
            }
        })
        .collect();

    Ok(projects)
}

#[derive(Deserialize)]
struct GitHubRepo {
    name: String,
    description: Option<String>,
    html_url: String,
    pushed_at: String,
    default_branch: Option<String>,
    language: Option<String>,
    topics: Option<Vec<String>>,
}

/// Detect tech stack from GitHub repo metadata
fn detect_github_tech_stack(repo: &GitHubRepo) -> Vec<String> {
    let mut stack = Vec::new();

    if let Some(lang) = &repo.language {
        stack.push(lang.clone());
    }

    if let Some(topics) = &repo.topics {
        for topic in topics.iter().take(3) {
            if !stack.contains(topic) {
                stack.push(topic.clone());
            }
        }
    }

    stack
}

/// Fetch repositories from GitLab API. Paginates via `x-next-page` header
/// and authenticates with a token via the `PRIVATE-TOKEN` header.
async fn fetch_gitlab_repos(
    username: &str,
    token: Option<&str>,
    max_repos: Option<usize>,
) -> Result<Vec<Project>, String> {
    let client = reqwest::Client::new();
    let base_url = format!(
        "https://gitlab.com/api/v4/users/{}/projects?per_page=100&order_by=last_activity_at",
        username
    );
    let mut page: u32 = 1;
    let mut all_repos: Vec<GitLabRepo> = Vec::new();

    loop {
        let url = format!("{}&page={}", base_url, page);
        let mut req = client.get(&url).header("User-Agent", "Mercator/1.0");
        if let Some(t) = token {
            req = req.header("PRIVATE-TOKEN", t);
        }
        let response = req
            .send()
            .await
            .map_err(|e| format!("GitLab request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(format_api_error("GitLab", status, &body, None, None));
        }

        let next_page_hdr = response
            .headers()
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let mut chunk: Vec<GitLabRepo> = response
            .json()
            .await
            .map_err(|e| format!("GitLab response was not a repo array: {}", e))?;

        if chunk.is_empty() {
            break;
        }
        all_repos.append(&mut chunk);

        if max_repos.is_some_and(|m| all_repos.len() >= m) {
            break;
        }

        match next_page_hdr.as_deref() {
            Some(next) if !next.is_empty() => {
                page = next.parse().unwrap_or(page + 1);
            }
            _ => break,
        }
    }

    let take = max_repos.unwrap_or(usize::MAX);
    let projects = all_repos
        .into_iter()
        .take(take)
        .map(|repo| {
            let tech_stack = detect_gitlab_tech_stack(&repo);
            Project {
                name: repo.name,
                path: repo.web_url.clone(),
                description: repo.description.unwrap_or_default(),
                project_type: ProjectType::GitLab,
                last_modified: Some(repo.last_activity_at),
                git_branch: Some(repo.default_branch.unwrap_or_else(|| "main".to_string())),
                last_commit: None,
                git_status: None,
                tech_stack,
                remote_url: Some(repo.web_url),
                agent_used: None,
                obsidian_url: None,
                obsidian_note_path: None,
                tags: vec![],
            }
        })
        .collect();

    Ok(projects)
}

#[derive(Deserialize)]
struct GitLabRepo {
    name: String,
    description: Option<String>,
    web_url: String,
    last_activity_at: String,
    default_branch: Option<String>,
    #[serde(rename = "tag_list")]
    tag_list: Option<Vec<String>>,
}

/// Detect tech stack from GitLab repo metadata
fn detect_gitlab_tech_stack(repo: &GitLabRepo) -> Vec<String> {
    let mut stack = Vec::new();

    if let Some(tags) = &repo.tag_list {
        for tag in tags.iter().take(3) {
            stack.push(tag.clone());
        }
    }

    stack
}

/// Detect tech stack based on project files
fn detect_tech_stack(path: &Path) -> Vec<String> {
    let mut stack = Vec::new();

    let tech_markers = [
        ("package.json", "Node.js"),
        ("Cargo.toml", "Rust"),
        ("go.mod", "Go"),
        ("requirements.txt", "Python"),
        ("Pipfile", "Python"),
        ("pyproject.toml", "Python"),
        ("Dockerfile", "Docker"),
        ("docker-compose.yml", "Docker"),
        ("docker-compose.yaml", "Docker"),
        ("Gemfile", "Ruby"),
        ("Cargo.lock", "Rust"),
        ("package-lock.json", "Node.js"),
        ("yarn.lock", "Yarn"),
        ("pnpm-lock.yaml", "pnpm"),
        ("pom.xml", "Java"),
        ("build.gradle", "Java"),
        ("composer.json", "PHP"),
        ("mix.exs", "Elixir"),
    ];

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().into_owned();

            for (marker, tech) in &tech_markers {
                if name == *marker && !stack.contains(&tech.to_string()) {
                    stack.push(tech.to_string());
                }
            }
        }
    }

    stack
}

/// Detect which AI coding agent was used in a project
fn detect_agent(path: &Path) -> Option<String> {
    // Claude Code markers
    if path.join("CLAUDE.md").exists() || path.join(".claude").exists() {
        return Some("claude".to_string());
    }
    // Codex markers
    if path.join("AGENTS.md").exists() || path.join(".codex").exists() {
        return Some("codex".to_string());
    }
    None
}

fn survey_projects(root: &Path) -> Vec<Project> {
    let mut projects = Vec::new();
    let mut it = WalkDir::new(root).into_iter();

    while let Some(Ok(entry)) = it.next() {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        if name == "target" || name == "node_modules" || name == ".git" {
            it.skip_current_dir();
            continue;
        }

        if path.is_dir() {
            let is_git = path.join(".git").exists();
            let idea_file = path.join("IDEA.md");

            if is_git || idea_file.exists() {
                let description = description_from_repo(path)
                    .unwrap_or_else(|| "No description provided.".to_string());

                let last_modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(format_time);

                let (git_branch, last_commit, git_status, remote_url) = if is_git {
                    get_git_info(path)
                } else {
                    (None, None, None, None)
                };

                let agent_used = detect_agent(path);
                projects.push(Project {
                    name,
                    path: path.to_string_lossy().into_owned(),
                    description,
                    project_type: if is_git {
                        ProjectType::Git
                    } else {
                        ProjectType::Idea
                    },
                    last_modified,
                    git_branch,
                    last_commit,
                    git_status,
                    tech_stack: detect_tech_stack(path),
                    remote_url,
                    agent_used,
                    obsidian_url: None,
                    obsidian_note_path: None,
                    tags: vec![],
                });

                it.skip_current_dir();
            } else if entry.depth() == 1 {
                projects.push(Project {
                    name,
                    path: path.to_string_lossy().into_owned(),
                    description: "Uncategorized directory".to_string(),
                    project_type: ProjectType::Folder,
                    last_modified: None,
                    git_branch: None,
                    last_commit: None,
                    git_status: None,
                    tech_stack: detect_tech_stack(path),
                    remote_url: None,
                    agent_used: detect_agent(path),
                    obsidian_url: None,
                    obsidian_note_path: None,
                    tags: vec![],
                });
            }
        }
    }
    projects
}

/// Normalize a remote URL for comparison (strip .git suffix, protocol, trailing slashes)
fn normalize_remote_url(url: &str) -> String {
    let mut url = url.trim().trim_end_matches('/').to_string();
    if url.ends_with(".git") {
        url.truncate(url.len() - 4);
    }
    // Convert SSH git@host:user/repo to host/user/repo
    if url.starts_with("git@") {
        url = url.strip_prefix("git@").unwrap().replacen(':', "/", 1);
    }
    // Remove protocol prefix
    for prefix in &["https://", "http://", "ssh://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            url = rest.to_string();
            break;
        }
    }
    url.to_lowercase()
}

/// Merge duplicate projects: when a local Git repo has the same remote URL as a
/// GitHub/GitLab repo, keep the local one and enrich it with remote metadata.
fn deduplicate_projects(projects: Vec<Project>) -> Vec<Project> {
    use std::collections::HashMap;

    // Index remote projects by normalized URL
    let mut remote_by_url: HashMap<String, Project> = HashMap::new();
    let mut local_projects: Vec<Project> = Vec::new();

    for p in projects {
        match p.project_type {
            ProjectType::GitHub | ProjectType::GitLab => {
                if let Some(ref url) = p.remote_url {
                    let key = normalize_remote_url(url);
                    remote_by_url.insert(key, p);
                } else {
                    // No URL to match on, keep as-is
                    local_projects.push(p);
                }
            }
            _ => {
                local_projects.push(p);
            }
        }
    }

    // Also index remote projects by name for fallback matching
    let mut remote_by_name: HashMap<String, String> = HashMap::new();
    for (url_key, p) in &remote_by_url {
        remote_by_name.insert(p.name.to_lowercase(), url_key.clone());
    }

    // For each local project, try to find and merge a matching remote
    let mut result: Vec<Project> = Vec::new();
    for mut local in local_projects {
        let matched_key = local
            .remote_url
            .as_ref()
            .map(|url| normalize_remote_url(url))
            .and_then(|key| {
                if remote_by_url.contains_key(&key) {
                    Some(key)
                } else {
                    None
                }
            })
            // Fallback: match by name for Folder types without remote URLs
            .or_else(|| remote_by_name.get(&local.name.to_lowercase()).cloned());

        if let Some(key) = matched_key {
            if let Some(remote) = remote_by_url.remove(&key) {
                remote_by_name.remove(&remote.name.to_lowercase());
                // Merge: local wins, but fill in gaps from remote
                if (local.description == "No description provided."
                    || local.description == "Uncategorized directory"
                    || local.description.starts_with('#'))
                    && !remote.description.is_empty()
                {
                    local.description = remote.description;
                }
                // Merge tech stacks
                for tech in &remote.tech_stack {
                    if !local.tech_stack.contains(tech) {
                        local.tech_stack.push(tech.clone());
                    }
                }
                // Keep the canonical remote URL
                if local.remote_url.is_none() {
                    local.remote_url = remote.remote_url;
                }
            }
        }
        result.push(local);
    }

    // Add any remaining remote projects that had no local match
    result.extend(remote_by_url.into_values());

    result
}

/// Sanitize a project name into a safe filename. Strips/replaces filesystem
/// hostile characters; collapses repeated separators; never returns an empty
/// string. Pure function, unit-tested.
fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => out.push('-'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let trimmed = out.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let collapsed: String = trimmed
        .chars()
        .scan(' ', |prev, c| {
            let keep = !(c == '-' && *prev == '-');
            *prev = c;
            Some((keep, c))
        })
        .filter(|(k, _)| *k)
        .map(|(_, c)| c)
        .collect();
    if collapsed.is_empty() {
        "untitled".to_string()
    } else {
        collapsed
    }
}

/// Render a Project as a markdown note with YAML frontmatter. Pure function
/// so it can be unit-tested without the filesystem.
fn render_project_markdown(p: &Project) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", yaml_escape(&p.name)));
    out.push_str(&format!("type: {:?}\n", p.project_type));
    out.push_str(&format!("path: {}\n", yaml_escape(&p.path)));
    if let Some(branch) = &p.git_branch {
        out.push_str(&format!("branch: {}\n", yaml_escape(branch)));
    }
    if let Some(status) = &p.git_status {
        out.push_str(&format!("status: {}\n", status));
    }
    if let Some(lm) = &p.last_modified {
        out.push_str(&format!("last_modified: {}\n", lm));
    }
    if let Some(remote) = &p.remote_url {
        out.push_str(&format!("remote: {}\n", yaml_escape(remote)));
    }
    if let Some(agent) = &p.agent_used {
        out.push_str(&format!("agent: {}\n", agent));
    }
    if !p.tech_stack.is_empty() {
        out.push_str(&format!(
            "tech: [{}]\n",
            p.tech_stack
                .iter()
                .map(|s| yaml_inline_string(s))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !p.tags.is_empty() {
        out.push_str(&format!(
            "tags: [{}]\n",
            p.tags
                .iter()
                .map(|s| yaml_inline_string(s))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(obs) = &p.obsidian_url {
        out.push_str(&format!("obsidian: {}\n", yaml_escape(obs)));
    }
    out.push_str("---\n\n");

    out.push_str(&format!("# {}\n\n", p.name));

    if !p.description.is_empty() && p.description != "No description provided." {
        out.push_str(&p.description);
        out.push_str("\n\n");
    }

    // Status section
    let mut status_lines = Vec::new();
    if let Some(b) = &p.git_branch {
        status_lines.push(format!("- **Branch**: `{}`", b));
    }
    if let Some(c) = &p.last_commit {
        status_lines.push(format!("- **Last commit**: {}", c));
    }
    if let Some(lm) = &p.last_modified {
        status_lines.push(format!("- **Last modified**: {}", lm));
    }
    if p.git_status.as_deref() == Some("uncommitted") {
        status_lines.push("- **Status**: dirty (uncommitted changes)".to_string());
    }
    if !status_lines.is_empty() {
        out.push_str("## Status\n\n");
        out.push_str(&status_lines.join("\n"));
        out.push_str("\n\n");
    }

    // Links — only durable URLs. The local path lives in frontmatter for
    // tooling; we don't add machine-specific `vscode://` links to a portable
    // markdown file.
    let mut links = Vec::new();
    if let Some(url) = &p.remote_url {
        links.push(format!("- [Remote]({})", url));
    }
    if let Some(obs) = &p.obsidian_url {
        links.push(format!("- [Obsidian note]({})", obs));
    }
    if !links.is_empty() {
        out.push_str("## Links\n\n");
        out.push_str(&links.join("\n"));
        out.push_str("\n\n");
    }

    // Tags + tech as a footer line
    if !p.tags.is_empty() || !p.tech_stack.is_empty() {
        out.push_str("---\n");
        if !p.tags.is_empty() {
            let s: Vec<String> = p.tags.iter().map(|t| format!("#{}", t)).collect();
            out.push_str(&format!("Tags: {}\n", s.join(" ")));
        }
        if !p.tech_stack.is_empty() {
            out.push_str(&format!("Stack: {}\n", p.tech_stack.join(", ")));
        }
    }

    out
}

/// Quote a string for a YAML scalar value if it contains special chars
fn yaml_escape(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.contains([':', '#', '"', '\'', '\n', '[', ']', '{', '}'])
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.starts_with('-');
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Quote a string for use inside a YAML inline list `[a, b, "c d"]`
fn yaml_inline_string(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('[') || s.contains(']') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Run the export command. Pure-ish (touches the filesystem); returns counts.
fn run_export(projects: &[Project], out_dir: &Path) -> Result<(usize, usize), String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("Cannot create {}: {}", out_dir.display(), e))?;
    let mut written = 0usize;
    let mut errors = 0usize;
    let mut seen_names: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in projects {
        let base = sanitize_filename(&p.name);
        // Disambiguate collisions by appending a counter
        let count = seen_names.entry(base.clone()).or_insert(0);
        let filename = if *count == 0 {
            format!("{}.md", base)
        } else {
            format!("{} ({}).md", base, count)
        };
        *count += 1;
        let target = out_dir.join(&filename);
        let body = render_project_markdown(p);
        if let Err(e) = std::fs::write(&target, body) {
            eprintln!("  ⚠  failed to write {}: {}", target.display(), e);
            errors += 1;
        } else {
            written += 1;
        }
    }
    Ok((written, errors))
}

fn save_map(projects: &[Project], output: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(projects)
        .map_err(|e| format!("Failed to serialize projects: {}", e))?;

    std::fs::write(output, &json)
        .map_err(|e| format!("Failed to write to {}: {}", output.display(), e))?;

    Ok(())
}

fn load_map(path: &Path) -> Result<Vec<Project>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    serde_json::from_str(&content).map_err(|e| format!("Failed to parse JSON: {}", e))
}

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

/// Strip simple inline markdown: [text](url) → text, **x** → x, `x` → x
fn strip_inline_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '[' {
            // [text](url) — keep text, skip url
            if let Some(close_brk) = s[i..].find("](") {
                let text_end = i + close_brk;
                if let Some(close_par) = s[text_end + 2..].find(')') {
                    out.push_str(&s[i + 1..text_end]);
                    i = text_end + 2 + close_par + 1;
                    continue;
                }
            }
        } else if c == '*' || c == '_' {
            // Skip emphasis markers (*, **, _, __)
            i += 1;
            while i < bytes.len() && (bytes[i] as char == c) {
                i += 1;
            }
            continue;
        } else if c == '`' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Extract a clean description paragraph from a markdown file.
/// Tries to skip frontmatter, badges, headings, HTML blocks, callouts, and
/// returns the first prose paragraph (joined to a single line) capped to ~240 chars.
fn extract_md_description(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines: Vec<&str> = content.lines().collect();

    // Strip YAML frontmatter: --- ... --- at the very top
    if lines.first().map(|l| l.trim() == "---").unwrap_or(false) {
        if let Some(end) = lines.iter().skip(1).position(|l| l.trim() == "---") {
            lines.drain(0..=end + 1);
        }
    }

    let is_skip = |line: &str| {
        let t = line.trim();
        t.is_empty()
            || t.starts_with('#')
            || t.starts_with("![")
            || t.starts_with("---")
            || t.starts_with("===")
            || t.starts_with("```")
            || t.starts_with('<')
            || t.starts_with("[!")
            || t.starts_with("- [")    // task lists / TOC
            || t.starts_with("* [")
            || (t.starts_with("[") && t.contains("]:")) // reference link defs
    };

    // Find first non-skipped line, then collect contiguous prose lines
    let mut paragraph = String::new();
    let mut started = false;
    for line in lines.iter() {
        let t = line.trim();
        if !started {
            if is_skip(t) {
                continue;
            }
            started = true;
        } else if t.is_empty() {
            break;
        } else if is_skip(t) {
            // Mid-paragraph skip-line ends the paragraph
            break;
        }
        // Strip blockquote and bullet prefixes
        let t = t.trim_start_matches('>').trim();
        let t = t.strip_prefix("- ").unwrap_or(t);
        let t = t.strip_prefix("* ").unwrap_or(t);
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(t);
    }

    if paragraph.is_empty() {
        return None;
    }
    let cleaned = strip_inline_md(&paragraph);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }

    // Cap at ~240 chars on a word boundary
    const MAX: usize = 240;
    let final_str = if cleaned.chars().count() > MAX {
        let mut end = cleaned
            .char_indices()
            .nth(MAX)
            .map(|(i, _)| i)
            .unwrap_or(cleaned.len());
        if let Some(space) = cleaned[..end].rfind(' ') {
            end = space;
        }
        format!("{}…", &cleaned[..end])
    } else {
        cleaned.to_string()
    };
    Some(final_str)
}

/// Read description from a directory by checking common markdown files in priority order
fn description_from_repo(path: &Path) -> Option<String> {
    for name in &["IDEA.md", "README.md", "CLAUDE.md", "AGENTS.md"] {
        let p = path.join(name);
        if p.exists() {
            if let Some(d) = extract_md_description(&p) {
                return Some(d);
            }
        }
    }
    None
}

/// Read the first meaningful content line from a markdown file
fn read_md_description(path: &Path) -> String {
    extract_md_description(path).unwrap_or_else(|| "No description".to_string())
}

/// Percent-encode a string for use in obsidian:// URIs
fn percent_encode(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                result.push(b as char);
            }
            b' ' => result.push_str("%20"),
            _ => result.push_str(&format!("%{:02X}", b)),
        }
    }
    result
}

/// Scan an Obsidian vault's Projects folder for idea/project notes
fn scan_obsidian_vault(vault_path: &Path, folder: &str, vault_name: &str) -> Vec<Project> {
    let projects_path = vault_path.join(folder);
    if !projects_path.exists() {
        eprintln!(
            "Warning: Obsidian projects folder not found: {}",
            projects_path.display()
        );
        return vec![];
    }

    let mut projects = Vec::new();

    let entries = match std::fs::read_dir(&projects_path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Warning: Cannot read {}: {}", projects_path.display(), e);
            return vec![];
        }
    };

    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy().into_owned();
        let entry_path = entry.path();

        if name.starts_with('.') {
            continue;
        }

        if entry_path.is_dir() {
            // Subfolder = project. Read first .md inside for description.
            let md_file = std::fs::read_dir(&entry_path).ok().and_then(|entries| {
                entries
                    .flatten()
                    .find(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
            });
            let description = md_file
                .as_ref()
                .map(|md| read_md_description(&md.path()))
                .unwrap_or_else(|| "Obsidian project folder".to_string());

            // Obsidian URI needs the file path without the final .md extension
            let relative = if let Some(ref md) = md_file {
                let md_name = md.file_name().to_string_lossy().into_owned();
                // Strip only the last .md (Obsidian strips one .md itself)
                let note_name = md_name.strip_suffix(".md").unwrap_or(&md_name);
                format!("{}/{}/{}", folder, name, note_name)
            } else {
                format!("{}/{}", folder, name)
            };
            let obsidian_url = format!(
                "obsidian://open?vault={}&file={}",
                percent_encode(vault_name),
                percent_encode(&relative)
            );

            let last_modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(format_time);

            projects.push(Project {
                name: name.clone(),
                path: entry_path.to_string_lossy().into_owned(),
                description,
                project_type: ProjectType::Obsidian,
                last_modified,
                git_branch: None,
                last_commit: None,
                git_status: None,
                tech_stack: vec![],
                remote_url: None,
                agent_used: None,
                obsidian_url: Some(obsidian_url),
                obsidian_note_path: Some(relative),
                tags: vec![],
            });
        } else if name.ends_with(".md") {
            if name == "@Projects.md" {
                // Parse bullet list as lightweight idea stubs
                if let Ok(content) = std::fs::read_to_string(&entry_path) {
                    for line in content.lines() {
                        let line = line.trim();
                        if let Some(idea) = line.strip_prefix("- ") {
                            let idea = idea.trim();
                            if idea.is_empty() {
                                continue;
                            }
                            let relative = format!("{}/{}", folder, "@Projects");
                            let obsidian_url = format!(
                                "obsidian://open?vault={}&file={}",
                                percent_encode(vault_name),
                                percent_encode(&relative)
                            );
                            projects.push(Project {
                                name: idea.to_string(),
                                path: entry_path.to_string_lossy().into_owned(),
                                description: "Idea from Obsidian vault".to_string(),
                                project_type: ProjectType::Obsidian,
                                last_modified: None,
                                git_branch: None,
                                last_commit: None,
                                git_status: None,
                                tech_stack: vec![],
                                remote_url: None,
                                agent_used: None,
                                obsidian_url: Some(obsidian_url),
                                obsidian_note_path: Some(relative.clone()),
                                tags: vec![],
                            });
                        }
                    }
                }
            } else {
                // Individual .md file = project note
                let project_name = name.strip_suffix(".md").unwrap_or(&name).to_string();
                let description = read_md_description(&entry_path);
                let relative = format!("{}/{}", folder, project_name);
                let obsidian_url = format!(
                    "obsidian://open?vault={}&file={}",
                    percent_encode(vault_name),
                    percent_encode(&relative)
                );

                let last_modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(format_time);

                projects.push(Project {
                    name: project_name,
                    path: entry_path.to_string_lossy().into_owned(),
                    description,
                    project_type: ProjectType::Obsidian,
                    last_modified,
                    git_branch: None,
                    last_commit: None,
                    git_status: None,
                    tech_stack: vec![],
                    remote_url: None,
                    agent_used: None,
                    obsidian_url: Some(obsidian_url),
                    obsidian_note_path: Some(relative),
                    tags: vec![],
                });
            }
        }
    }

    projects
}

/// Normalize a name for fuzzy matching (lowercase, strip hyphens/underscores/spaces)
fn normalize_name(name: &str) -> String {
    name.to_lowercase().replace(['-', '_', ' '], "")
}

/// Link Obsidian notes to existing projects by name matching.
/// Matched Obsidian entries merge their obsidian_url into the existing project and are removed.
fn link_obsidian_notes(projects: Vec<Project>) -> Vec<Project> {
    use std::collections::HashMap;

    let mut obsidian_by_name: HashMap<String, Project> = HashMap::new();
    let mut others: Vec<Project> = Vec::new();

    for p in projects {
        if matches!(p.project_type, ProjectType::Obsidian) {
            obsidian_by_name.insert(normalize_name(&p.name), p);
        } else {
            others.push(p);
        }
    }

    let mut result: Vec<Project> = Vec::new();
    for mut proj in others {
        let key = normalize_name(&proj.name);
        if let Some(obs) = obsidian_by_name.remove(&key) {
            proj.obsidian_url = obs.obsidian_url;
            proj.obsidian_note_path = obs.obsidian_note_path;
        }
        result.push(proj);
    }

    // Remaining unmatched Obsidian projects stay as standalone
    result.extend(obsidian_by_name.into_values());
    result
}

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

/// Auto-tag projects based on name, description, and tech stack
fn auto_tag_projects(projects: &mut [Project]) {
    let tag_keywords: &[(&str, &[&str])] = &[
        (
            "ai",
            &[
                "ai",
                "llm",
                "gpt",
                "claude",
                "codex",
                "agent",
                "neural",
                "ml",
                "machine learning",
                "openai",
                "anthropic",
                "pytorch",
                "tensorflow",
            ],
        ),
        (
            "web",
            &[
                "website",
                "landing",
                "frontend",
                "react",
                "next",
                "svelte",
                "html",
                "css",
                "tailwind",
                "vue",
                "angular",
                "sveltekit",
            ],
        ),
        (
            "api",
            &[
                "api", "rest", "graphql", "endpoint", "server", "express", "axum", "hono",
                "fastapi",
            ],
        ),
        (
            "cli",
            &[
                "cli",
                "terminal",
                "command line",
                "command-line",
                "shell",
                "bash",
            ],
        ),
        (
            "devops",
            &[
                "docker",
                "kubernetes",
                "k8s",
                "ci/cd",
                "deploy",
                "infra",
                "terraform",
                "ansible",
                "helm",
                "umbrel",
            ],
        ),
        (
            "mobile",
            &[
                "ios",
                "android",
                "mobile",
                "swift",
                "kotlin",
                "react native",
                "flutter",
            ],
        ),
        (
            "data",
            &[
                "database",
                "postgres",
                "sqlite",
                "redis",
                "mongo",
                "data",
                "analytics",
                "scraping",
                "crawler",
            ],
        ),
        (
            "blockchain",
            &[
                "blockchain",
                "web3",
                "solana",
                "ethereum",
                "crypto",
                "token",
                "nft",
                "rwa",
                "smart contract",
            ],
        ),
        (
            "seo",
            &["seo", "search engine", "sitemap", "analytics", "tracking"],
        ),
        (
            "auth",
            &[
                "auth",
                "login",
                "oauth",
                "jwt",
                "session",
                "credential",
                "password",
            ],
        ),
        (
            "bot",
            &[
                "bot",
                "telegram",
                "whatsapp",
                "discord",
                "slack",
                "chat",
                "messaging",
            ],
        ),
        (
            "automation",
            &[
                "automat", "workflow", "cron", "schedule", "scrape", "crawl", "hook",
            ],
        ),
        ("game", &["game", "rpg", "player", "level", "score"]),
        (
            "docs",
            &[
                "documentation",
                "readme",
                "wiki",
                "knowledge",
                "obsidian",
                "note",
            ],
        ),
        (
            "finance",
            &[
                "tax",
                "invoice",
                "payment",
                "billing",
                "finance",
                "accounting",
                "portfolio",
            ],
        ),
    ];

    for project in projects.iter_mut() {
        let haystack = format!(
            "{} {} {} {}",
            project.name.to_lowercase(),
            project.description.to_lowercase(),
            project.tech_stack.join(" ").to_lowercase(),
            project.agent_used.as_deref().unwrap_or(""),
        );

        let mut tags = Vec::new();
        for (tag, keywords) in tag_keywords {
            if keywords.iter().any(|kw| haystack.contains(kw)) {
                tags.push(tag.to_string());
            }
        }
        project.tags = tags;
    }
}

/// Extract significant keywords from a description for domain matching
fn domain_keywords(text: &str) -> Vec<String> {
    let stopwords = [
        "the",
        "and",
        "for",
        "with",
        "from",
        "that",
        "this",
        "are",
        "was",
        "not",
        "you",
        "your",
        "has",
        "have",
        "had",
        "but",
        "all",
        "can",
        "will",
        "one",
        "our",
        "out",
        "use",
        "how",
        "its",
        "let",
        "may",
        "who",
        "did",
        "get",
        "she",
        "her",
        "him",
        "his",
        "old",
        "new",
        "now",
        "way",
        "each",
        "make",
        "like",
        "just",
        "over",
        "such",
        "take",
        "than",
        "them",
        "very",
        "when",
        "what",
        "some",
        "into",
        "been",
        "more",
        "other",
        "which",
        "about",
        "would",
        "their",
        "these",
        "could",
        "project",
        "using",
        "based",
        "built",
        "also",
        "here",
        "https",
        "http",
        "www",
        "com",
        "org",
        "github",
        "description",
        "provided",
        "none",
        "file",
        "code",
    ];
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4 && !stopwords.contains(w))
        .map(|w| w.to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

/// Compute a relationship graph from projects based on goals and usage, not just tech
fn compute_graph(projects: &[Project]) -> serde_json::Value {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // Pre-compute per-project data
    let keywords: Vec<Vec<String>> = projects
        .iter()
        .map(|p| {
            // Cap description to 200 chars to avoid noisy long descriptions
            let desc_cap: String = p.description.chars().take(200).collect();
            domain_keywords(&format!("{} {}", p.name, desc_cap))
        })
        .collect();

    // Build nodes
    for (i, p) in projects.iter().enumerate() {
        nodes.push(serde_json::json!({
            "id": i,
            "name": p.name,
            "type": p.project_type,
            "tags": p.tags,
            "tech": p.tech_stack,
            "description": p.description,
            "agent": p.agent_used,
            "path": p.path,
            "obsidianUrl": p.obsidian_url,
        }));
    }

    for i in 0..projects.len() {
        for j in (i + 1)..projects.len() {
            let mut weight: f32 = 0.0;
            let mut reasons = Vec::new();

            // 1. One project name appears in the other's description (usage/dependency)
            let ni = projects[i].name.to_lowercase();
            let nj = projects[j].name.to_lowercase();
            let di = projects[i].description.to_lowercase();
            let dj = projects[j].description.to_lowercase();
            if ni.len() >= 4 && dj.contains(&ni) {
                weight += 6.0;
                reasons.push(format!(
                    "{} mentioned in {}",
                    projects[i].name, projects[j].name
                ));
            }
            if nj.len() >= 4 && di.contains(&nj) {
                weight += 6.0;
                reasons.push(format!(
                    "{} mentioned in {}",
                    projects[j].name, projects[i].name
                ));
            }

            // 3. Shared domain keywords from descriptions (goal similarity)
            let shared_kw: Vec<&String> = keywords[i]
                .iter()
                .filter(|kw| keywords[j].contains(kw))
                .collect();
            let kw_score = (shared_kw.len() as f32).min(5.0);
            if kw_score >= 3.0 {
                weight += kw_score;
                reasons.push(format!(
                    "shared: {}",
                    shared_kw
                        .iter()
                        .take(3)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            // 4. Shared tags — only count if there's already another signal
            //    (prevents generic tag-only connections like "both tagged ai")
            let shared_tags: Vec<&String> = projects[i]
                .tags
                .iter()
                .filter(|t| projects[j].tags.contains(t))
                .collect();
            if !shared_tags.is_empty() && weight > 0.0 {
                weight += shared_tags.len() as f32;
                reasons.push(format!(
                    "tags: {}",
                    shared_tags
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }

            // 5. Obsidian idea linked to implementation
            if (matches!(projects[i].project_type, ProjectType::Obsidian)
                && !matches!(projects[j].project_type, ProjectType::Obsidian))
                || (!matches!(projects[i].project_type, ProjectType::Obsidian)
                    && matches!(projects[j].project_type, ProjectType::Obsidian))
            {
                // Already linked via name matching — check if remaining connection exists
                if weight > 0.0 {
                    weight += 3.0;
                    reasons.push("idea→impl".to_string());
                }
            }

            // Only include meaningful connections
            if weight >= 4.0 {
                edges.push(serde_json::json!({
                    "source": i,
                    "target": j,
                    "weight": weight,
                    "shared": reasons,
                }));
            }
        }
    }

    serde_json::json!({ "nodes": nodes, "edges": edges })
}

// ── Agent Runner (Swarm Integration) ───────────────────────────────────
//
// All agent runner code is gated on the `swarm` feature so the binary builds
// and ships without the local `../swarm` workspace member. Enable with
// `cargo build --features swarm` for the full experience.

#[cfg(feature = "swarm")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentJob {
    id: String,
    project_name: String,
    project_path: String,
    prompt: String,
    model: String,
    permission_mode: String,
    max_budget_usd: f64,
    status: String, // "running", "succeeded", "failed", "cancelled"
    started_at: String,
    finished_at: Option<String>,
    cost_usd: f64,
    tool_calls: u32,
    input_tokens: u64,
    output_tokens: u64,
    summary: Option<String>,
    error: Option<String>,
    branch: Option<String>,
    changed_files: Vec<String>,
}

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

fn write_purged(map_file: &Path, set: &std::collections::HashSet<String>) -> Result<(), String> {
    let path = purge_file_path(map_file);
    let mut list: Vec<&String> = set.iter().collect();
    list.sort();
    let json = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?;
    std::fs::write(&path, &json).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

#[derive(Deserialize)]
struct PurgeRequest {
    path: String,
}

async fn purge_project_api(
    State(state): State<AppState>,
    Json(req): Json<PurgeRequest>,
) -> Json<serde_json::Value> {
    let projects = match load_map(&state.map_file) {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    let before = projects.len();
    let target = req.path.trim_end_matches('/').to_string();
    let kept: Vec<Project> = projects
        .into_iter()
        .filter(|p| p.path.trim_end_matches('/') != target)
        .collect();
    let removed = before - kept.len();

    let mut purged = read_purged(&state.map_file);
    purged.insert(target.clone());
    if let Err(e) = write_purged(&state.map_file, &purged) {
        return Json(serde_json::json!({ "ok": false, "error": e }));
    }
    if let Err(e) = save_map(&kept, &state.map_file) {
        return Json(serde_json::json!({ "ok": false, "error": e }));
    }
    Json(
        serde_json::json!({ "ok": true, "removed": removed, "remaining": kept.len(), "purged_total": purged.len() }),
    )
}

async fn purged_list_api(State(state): State<AppState>) -> Json<Vec<String>> {
    let mut list: Vec<String> = read_purged(&state.map_file).into_iter().collect();
    list.sort();
    Json(list)
}

#[derive(Deserialize)]
struct RestoreRequest {
    path: String,
}

async fn restore_project_api(
    State(state): State<AppState>,
    Json(req): Json<RestoreRequest>,
) -> Json<serde_json::Value> {
    let mut purged = read_purged(&state.map_file);
    let target = req.path.trim_end_matches('/').to_string();
    let removed = purged.remove(&target);
    if let Err(e) = write_purged(&state.map_file, &purged) {
        return Json(serde_json::json!({ "ok": false, "error": e }));
    }
    Json(
        serde_json::json!({ "ok": true, "removed_from_blocklist": removed, "remaining": purged.len() }),
    )
}

/// API endpoint: re-run auto-categorization against the existing map and save it
async fn recategorize_api(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut projects = match load_map(&state.map_file) {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    auto_tag_projects(&mut projects);
    if let Err(e) = save_map(&projects, &state.map_file) {
        return Json(serde_json::json!({ "ok": false, "error": e }));
    }
    let count = projects.len();
    let tagged = projects.iter().filter(|p| !p.tags.is_empty()).count();
    Json(serde_json::json!({ "ok": true, "projects": count, "tagged": tagged }))
}

#[cfg(feature = "swarm")]
#[derive(Deserialize)]
struct AgentRunRequest {
    project_path: String,
    prompt: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_permission_mode")]
    permission_mode: String,
    #[serde(default = "default_budget")]
    max_budget_usd: f64,
}

#[cfg(feature = "swarm")]
fn default_model() -> String {
    "sonnet".to_string()
}
#[cfg(feature = "swarm")]
fn default_permission_mode() -> String {
    "acceptEdits".to_string()
}
#[cfg(feature = "swarm")]
fn default_budget() -> f64 {
    1.0
}

#[cfg(feature = "swarm")]
async fn agent_run(
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

#[cfg(feature = "swarm")]
async fn agent_cancel(
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

#[cfg(feature = "swarm")]
struct SwarmOutcome {
    cost_usd: f64,
    tool_calls: u32,
    input_tokens: u64,
    output_tokens: u64,
    summary: Option<String>,
    branch: Option<String>,
    changed_files: Vec<String>,
}

#[cfg(feature = "swarm")]
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

#[cfg(feature = "swarm")]
async fn agent_jobs(State(state): State<AppState>) -> Json<Vec<AgentJob>> {
    let jobs = state.jobs.lock().await;
    Json(jobs.clone())
}

#[cfg(feature = "swarm")]
async fn agent_job_detail(
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

#[cfg(feature = "swarm")]
async fn agent_job_log(
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

// ── Skills Inventory ───────────────────────────────────────────────────

#[derive(Serialize)]
struct SkillUsage {
    project: String,
    path: String,
    skill_path: String,
    hash: u64,
    /// Status compared to the global copy: "synced", "diverged", or "no-global"
    status: String,
}

#[derive(Serialize)]
struct SkillEntry {
    name: String,
    description: String,
    /// Frontmatter `version` if present, otherwise plugin version from path
    version: Option<String>,
    /// True if a global copy exists at ~/.claude/skills/<name>/
    has_global: bool,
    global_hash: Option<u64>,
    global_path: Option<String>,
    /// Per-project usages (already includes global=false)
    projects: Vec<SkillUsage>,
    /// Inferred group: marketplace name for plugin skills, name prefix otherwise
    group: String,
    /// Source repo URL (from frontmatter or marketplace manifest)
    repo_url: Option<String>,
}

#[derive(Serialize)]
struct SkillGroup {
    name: String,
    repo_url: Option<String>,
    skills: Vec<SkillEntry>,
}

fn fnv_hash(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    repository: Option<String>,
}

/// Parse YAML frontmatter from a SKILL.md file
fn parse_skill_frontmatter(content: &str) -> SkillFrontmatter {
    let mut fm = SkillFrontmatter {
        name: None,
        description: None,
        version: None,
        repository: None,
    };
    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return fm;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        let strip = |s: &str| s.trim().trim_matches('"').trim_matches('\'').to_string();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            fm.name = Some(strip(rest));
        } else if let Some(rest) = trimmed.strip_prefix("description:") {
            fm.description = Some(strip(rest));
        } else if let Some(rest) = trimmed.strip_prefix("version:") {
            fm.version = Some(strip(rest));
        } else if let Some(rest) = trimmed.strip_prefix("repository:") {
            fm.repository = Some(strip(rest));
        } else if let Some(rest) = trimmed.strip_prefix("repo:") {
            fm.repository = Some(strip(rest));
        } else if let Some(rest) = trimmed.strip_prefix("source:") {
            fm.repository = Some(strip(rest));
        }
    }
    fm
}

struct SkillRead {
    name: String,
    description: String,
    version: Option<String>,
    repository: Option<String>,
    hash: u64,
}

/// Read a skill from a directory containing SKILL.md
fn read_skill(skill_dir: &Path) -> Option<SkillRead> {
    let skill_md = skill_dir.join("SKILL.md");
    let content = std::fs::read_to_string(&skill_md).ok()?;
    let fm = parse_skill_frontmatter(&content);
    let name = fm.name.unwrap_or_else(|| {
        skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string())
    });
    Some(SkillRead {
        name,
        description: fm.description.unwrap_or_default(),
        version: fm.version,
        repository: fm.repository,
        hash: fnv_hash(&content),
    })
}

/// Scan a `.claude/skills` directory and return its skills
fn scan_skills_dir(skills_root: &Path) -> Vec<(SkillRead, PathBuf)> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(skills_root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(s) = read_skill(&p) {
                    results.push((s, p));
                }
            }
        }
    }
    results
}

/// Read marketplace name → repo URL map from ~/.claude/plugins/known_marketplaces.json
fn load_marketplace_repos(home: &Path) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return map;
    };
    let Ok(json): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
        return map;
    };
    let Some(obj) = json.as_object() else {
        return map;
    };
    for (mp_name, info) in obj {
        let source = info.get("source").and_then(|s| s.as_object());
        if let Some(s) = source {
            let kind = s.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let repo = s.get("repo").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "github" && !repo.is_empty() {
                map.insert(mp_name.clone(), format!("https://github.com/{}", repo));
            } else if !repo.is_empty() {
                map.insert(mp_name.clone(), repo.to_string());
            }
        }
    }
    map
}

/// Walk plugin cache: ~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/skills/<skill>/SKILL.md
/// Returns (marketplace, plugin_version, SkillRead, skill_path)
fn scan_plugin_skills(home: &Path) -> Vec<(String, String, SkillRead, PathBuf)> {
    let mut out = Vec::new();
    let cache_root = home.join(".claude/plugins/cache");
    let Ok(marketplaces) = std::fs::read_dir(&cache_root) else {
        return out;
    };
    for mp_entry in marketplaces.flatten() {
        let mp_name = mp_entry.file_name().to_string_lossy().into_owned();
        let mp_path = mp_entry.path();
        if !mp_path.is_dir() {
            continue;
        }
        // Walk plugins in this marketplace
        let Ok(plugins) = std::fs::read_dir(&mp_path) else {
            continue;
        };
        for plugin_entry in plugins.flatten() {
            let plugin_path = plugin_entry.path();
            if !plugin_path.is_dir() {
                continue;
            }
            // Walk versions
            let Ok(versions) = std::fs::read_dir(&plugin_path) else {
                continue;
            };
            for version_entry in versions.flatten() {
                let version_path = version_entry.path();
                if !version_path.is_dir() {
                    continue;
                }
                let version = version_entry.file_name().to_string_lossy().into_owned();
                let skills_root = version_path.join("skills");
                if !skills_root.exists() {
                    continue;
                }
                for (mut sr, sp) in scan_skills_dir(&skills_root) {
                    if sr.version.is_none() {
                        sr.version = Some(version.clone());
                    }
                    out.push((mp_name.clone(), version.clone(), sr, sp));
                }
            }
        }
    }
    out
}

/// Derive a group name from a skill name. Returns first hyphen segment if it's
/// a meaningful prefix, else "core". Caller decides whether the prefix has
/// enough siblings to form a real group.
fn name_prefix_group(name: &str) -> String {
    // Handle plugin-namespaced form: "plugin:skill" → "plugin"
    if let Some((before, _)) = name.split_once(':') {
        return before.to_string();
    }
    if let Some((before, _)) = name.split_once('-') {
        if before.len() >= 2 {
            return before.to_string();
        }
    }
    "core".to_string()
}

async fn skills_api(State(state): State<AppState>) -> Json<Vec<SkillGroup>> {
    use std::collections::HashMap;

    let home = dirs::home_dir().unwrap_or_default();
    let marketplace_repos = load_marketplace_repos(&home);
    let mut entries: HashMap<String, SkillEntry> = HashMap::new();

    // 1. Global skills at ~/.claude/skills/
    for (sr, path) in scan_skills_dir(&home.join(".claude/skills")) {
        entries.insert(
            sr.name.clone(),
            SkillEntry {
                name: sr.name,
                description: sr.description,
                version: sr.version,
                has_global: true,
                global_hash: Some(sr.hash),
                global_path: Some(path.to_string_lossy().into_owned()),
                projects: Vec::new(),
                group: String::new(),    // assigned later
                repo_url: sr.repository, // may get overridden by marketplace lookup
            },
        );
    }

    // 2. Plugin skills (in cache) — these may already exist as global copies; merge
    for (mp_name, _version, sr, path) in scan_plugin_skills(&home) {
        let repo = marketplace_repos.get(&mp_name).cloned();
        let entry = entries
            .entry(sr.name.clone())
            .or_insert_with(|| SkillEntry {
                name: sr.name.clone(),
                description: sr.description.clone(),
                version: sr.version.clone(),
                has_global: false,
                global_hash: None,
                global_path: None,
                projects: Vec::new(),
                group: mp_name.clone(),
                repo_url: repo.clone(),
            });
        // Prefer marketplace-provided repo + mark group
        entry.group = mp_name.clone();
        if entry.repo_url.is_none() {
            entry.repo_url = repo;
        }
        if entry.global_path.is_none() {
            entry.global_path = Some(path.to_string_lossy().into_owned());
        }
        if entry.version.is_none() {
            entry.version = sr.version;
        }
    }

    // 3. Project-level skills
    let projects = load_map(&state.map_file).unwrap_or_default();
    for project in projects.iter() {
        if !matches!(
            project.project_type,
            ProjectType::Git | ProjectType::Folder | ProjectType::Idea
        ) {
            continue;
        }
        let pskills = PathBuf::from(&project.path).join(".claude/skills");
        if !pskills.exists() {
            continue;
        }
        for (sr, skill_path) in scan_skills_dir(&pskills) {
            let entry = entries
                .entry(sr.name.clone())
                .or_insert_with(|| SkillEntry {
                    name: sr.name.clone(),
                    description: sr.description.clone(),
                    version: sr.version.clone(),
                    has_global: false,
                    global_hash: None,
                    global_path: None,
                    projects: Vec::new(),
                    group: String::new(),
                    repo_url: sr.repository.clone(),
                });
            if entry.description.is_empty() && !sr.description.is_empty() {
                entry.description = sr.description.clone();
            }
            if entry.repo_url.is_none() && sr.repository.is_some() {
                entry.repo_url = sr.repository;
            }
            let status = match entry.global_hash {
                Some(g) if g == sr.hash => "synced",
                Some(_) => "diverged",
                None => "no-global",
            }
            .to_string();
            entry.projects.push(SkillUsage {
                project: project.name.clone(),
                path: project.path.clone(),
                skill_path: skill_path.to_string_lossy().into_owned(),
                hash: sr.hash,
                status,
            });
        }
    }

    // 4. Assign groups: skills already tagged with marketplace stay; rest get
    //    name-prefix grouping iff ≥2 skills share the prefix
    let mut prefix_counts: HashMap<String, usize> = HashMap::new();
    for entry in entries.values() {
        if entry.group.is_empty() {
            *prefix_counts
                .entry(name_prefix_group(&entry.name))
                .or_insert(0) += 1;
        }
    }
    for entry in entries.values_mut() {
        if entry.group.is_empty() {
            let prefix = name_prefix_group(&entry.name);
            entry.group =
                if prefix == "core" || prefix_counts.get(&prefix).copied().unwrap_or(0) < 2 {
                    "core".to_string()
                } else {
                    prefix
                };
        }
    }

    // 5. Bucket into groups
    let mut group_map: HashMap<String, SkillGroup> = HashMap::new();
    for (_, entry) in entries.into_iter() {
        let group_name = entry.group.clone();
        let group_repo = entry.repo_url.clone();
        let g = group_map
            .entry(group_name.clone())
            .or_insert_with(|| SkillGroup {
                name: group_name,
                repo_url: None,
                skills: Vec::new(),
            });
        if g.repo_url.is_none() && group_repo.is_some() {
            g.repo_url = group_repo;
        }
        g.skills.push(entry);
    }

    // Sort skills inside each group; then sort groups
    let mut groups: Vec<SkillGroup> = group_map.into_values().collect();
    for g in groups.iter_mut() {
        g.skills.sort_by(|a, b| {
            b.projects
                .len()
                .cmp(&a.projects.len())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }
    groups.sort_by(|a, b| {
        // "core" goes last; otherwise by skill count desc, then name
        let a_core = a.name == "core";
        let b_core = b.name == "core";
        a_core
            .cmp(&b_core)
            .then_with(|| b.skills.len().cmp(&a.skills.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Json(groups)
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
        } => {
            // Default to "." when no paths are given
            if paths.is_empty() {
                paths.push(PathBuf::from("."));
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

                if let Some(gh_user) = &github {
                    let auth_label = if github_token.is_some() {
                        " (authenticated)"
                    } else {
                        " (unauthenticated, 60/hr cap)"
                    };
                    eprintln!("Fetching GitHub repos for {}{}...", gh_user, auth_label);
                    match fetch_github_repos(gh_user, github_token.as_deref(), max_repos).await {
                        Ok(gh_repos) => {
                            eprintln!("  fetched {} GitHub repos", gh_repos.len());
                            all_projects.extend(gh_repos);
                        }
                        Err(e) => {
                            eprintln!("  ⚠  {}", e);
                        }
                    }
                }

                if let Some(gl_user) = &gitlab {
                    let auth_label = if gitlab_token.is_some() {
                        " (authenticated)"
                    } else {
                        " (unauthenticated)"
                    };
                    eprintln!("Fetching GitLab repos for {}{}...", gl_user, auth_label);
                    match fetch_gitlab_repos(gl_user, gitlab_token.as_deref(), max_repos).await {
                        Ok(gl_repos) => {
                            eprintln!("  fetched {} GitLab repos", gl_repos.len());
                            all_projects.extend(gl_repos);
                        }
                        Err(e) => {
                            eprintln!("  ⚠  {}", e);
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

                // Filter out purged paths so they stay gone across re-surveys
                let purged = read_purged(&output);
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

                match watch {
                    Some(minutes) => {
                        eprintln!("Next scan in {} min. Press Ctrl+C to stop.", minutes);
                        tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)).await;
                    }
                    None => break,
                }
            }
        }
        Commands::Export {
            out_dir,
            map_file,
            obsidian_vault,
            obsidian_folder,
        } => {
            let projects = match load_map(&map_file) {
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
        Commands::Serve {
            port,
            bind,
            map_file,
        } => {
            // Read the map file on each request so browser refresh picks up changes
            let map_path = map_file.clone();
            let serve_map = move || {
                let path = map_path.clone();
                async move {
                    match load_map(&path) {
                        Ok(projects) => Json(projects),
                        Err(e) => {
                            eprintln!("Warning: {}", e);
                            Json(vec![])
                        }
                    }
                }
            };

            let graph_path = map_file.clone();
            let serve_graph = move || {
                let path = graph_path.clone();
                async move {
                    match load_map(&path) {
                        Ok(projects) => Json(compute_graph(&projects)),
                        Err(_) => Json(serde_json::json!({ "nodes": [], "edges": [] })),
                    }
                }
            };

            let app_state = AppState {
                #[cfg(feature = "swarm")]
                jobs: Arc::new(Mutex::new(Vec::new())),
                #[cfg(feature = "swarm")]
                task_handles: Arc::new(Mutex::new(std::collections::HashMap::new())),
                map_file: map_file.clone(),
            };

            // /api/* routes are protected by an optional Bearer token
            // (set MERCATOR_TOKEN). Static dist/ files are served without auth
            // since the dashboard HTML itself is public; the API is the
            // sensitive surface.
            let api = Router::new()
                .route("/api/map", get(serve_map))
                .route("/api/graph", get(serve_graph))
                .route("/api/open-terminal", post(open_terminal))
                .route("/api/git-status", get(get_git_status_api))
                .route("/api/categorize", post(recategorize_api))
                .route("/api/skills", get(skills_api))
                .route("/api/project/purge", post(purge_project_api))
                .route("/api/project/restore", post(restore_project_api))
                .route("/api/purged", get(purged_list_api))
                .route("/api/project/tree", get(project_tree_api))
                .route("/api/project/file", get(project_file_api));

            #[cfg(feature = "swarm")]
            let api = api
                .route("/api/agent/run", post(agent_run))
                .route("/api/agent/jobs", get(agent_jobs))
                .route("/api/agent/job/{id}", get(agent_job_detail))
                .route("/api/agent/job/{id}/log", get(agent_job_log))
                .route("/api/agent/job/{id}/cancel", post(agent_cancel));

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
        let local = project(
            "mercator",
            ProjectType::Git,
            Some("git@github.com:zot24/mercator.git"),
        );
        let mut remote = project(
            "mercator",
            ProjectType::GitHub,
            Some("https://github.com/zot24/mercator"),
        );
        remote.description = "Cartography for your local landscape".to_string();
        remote.tech_stack = vec!["Rust".to_string()];

        let merged = deduplicate_projects(vec![local, remote]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].project_type, ProjectType::Git));
        assert_eq!(
            merged[0].description,
            "Cartography for your local landscape"
        );
        assert!(merged[0].tech_stack.contains(&"Rust".to_string()));
    }

    #[test]
    fn deduplicate_keeps_remote_only_projects() {
        let only_remote = project("foo", ProjectType::GitHub, Some("https://github.com/x/foo"));
        let merged = deduplicate_projects(vec![only_remote]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0].project_type, ProjectType::GitHub));
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
}
