// Mercator - Project Topography Tool
// A Rust CLI tool for discovering and visualizing your local development projects

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::process::Command;
use walkdir::WalkDir;
use axum::{extract::State, routing::{get, post}, Json, Router};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;

#[derive(Parser)]
#[command(name = "mercator")]
#[command(about = "Cartography for your local projects", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Survey a directory for projects
    Survey {
        /// Path to survey (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output file for the survey results
        #[arg(short, long, default_value = "mercator_map.json")]
        output: PathBuf,

        /// GitHub username to fetch repos from
        #[arg(long)]
        github: Option<String>,

        /// GitLab username to fetch repos from
        #[arg(long)]
        gitlab: Option<String>,

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
    #[serde(rename = "obsidianUrl", default, skip_serializing_if = "Option::is_none")]
    obsidian_url: Option<String>,
    /// Relative path to the Obsidian note within the vault
    #[serde(rename = "obsidianNotePath", default, skip_serializing_if = "Option::is_none")]
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
fn get_git_info(path: &Path) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
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

/// Fetch repositories from GitHub API
async fn fetch_github_repos(username: &str) -> Vec<Project> {
    let mut projects = Vec::new();
    
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/users/{}/repos?per_page=100&sort=pushed", username);
    
    match client.get(&url)
        .header("User-Agent", "Mercator/1.0")
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
    {
        Ok(response) => {
            if let Ok(repos) = response.json::<Vec<GitHubRepo>>().await {
                for repo in repos.into_iter().take(50) {
                    let tech_stack = detect_github_tech_stack(&repo);
                    
                    projects.push(Project {
                        name: repo.name.clone(),
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
                    });
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: Failed to fetch GitHub repos: {}", e);
        }
    }
    
    projects
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

/// Fetch repositories from GitLab API
async fn fetch_gitlab_repos(username: &str) -> Vec<Project> {
    let mut projects = Vec::new();
    
    let client = reqwest::Client::new();
    let url = format!("https://gitlab.com/api/v4/users/{}/projects?per_page=50&order_by=pushed_at", username);
    
    match client.get(&url)
        .header("User-Agent", "Mercator/1.0")
        .send()
        .await
    {
        Ok(response) => {
            if let Ok(repos) = response.json::<Vec<GitLabRepo>>().await {
                for repo in repos.into_iter().take(50) {
                    let tech_stack = detect_gitlab_tech_stack(&repo);
                    
                    projects.push(Project {
                        name: repo.name.clone(),
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
                    });
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: Failed to fetch GitLab repos: {}", e);
        }
    }
    
    projects
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
                if name == *marker {
                    if !stack.contains(&tech.to_string()) {
                        stack.push(tech.to_string());
                    }
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
        let name = path.file_name()
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
            let readme_file = path.join("README.md");

            if is_git || idea_file.exists() {
                let mut description = String::from("No description provided.");
                let desc_path = if idea_file.exists() { idea_file } else { readme_file };
                
                if desc_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&desc_path) {
                        // Find the first non-empty, non-heading, non-HTML line as description
                        description = content.lines()
                            .map(|l| l.trim())
                            .filter(|l| {
                                !l.is_empty()
                                    && !l.starts_with('#')
                                    && !l.starts_with("![")
                                    && !l.starts_with("---")
                                    && !l.starts_with('<')
                                    && !l.starts_with("[!")
                            })
                            .next()
                            .unwrap_or("No description provided.")
                            .to_string();
                        // Strip markdown formatting like > blockquotes
                        if description.starts_with('>') {
                            description = description.trim_start_matches('>').trim().to_string();
                        }
                    }
                }

                let last_modified = entry.metadata().ok()
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
                    project_type: if is_git { ProjectType::Git } else { ProjectType::Idea },
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
        let matched_key = local.remote_url.as_ref()
            .map(|url| normalize_remote_url(url))
            .and_then(|key| {
                if remote_by_url.contains_key(&key) { Some(key) } else { None }
            })
            // Fallback: match by name for Folder types without remote URLs
            .or_else(|| remote_by_name.get(&local.name.to_lowercase()).cloned());

        if let Some(key) = matched_key {
            if let Some(remote) = remote_by_url.remove(&key) {
                remote_by_name.remove(&remote.name.to_lowercase());
                // Merge: local wins, but fill in gaps from remote
                if local.description == "No description provided."
                    || local.description == "Uncategorized directory"
                    || local.description.starts_with('#')
                {
                    if !remote.description.is_empty() {
                        local.description = remote.description;
                    }
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
    
    serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse JSON: {}", e))
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
        Ok(output) if output.status.success() => {
            Json(OpenTerminalResponse { success: true, error: None })
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Json(OpenTerminalResponse {
                success: false,
                error: Some(stderr),
            })
        }
        Err(e) => {
            Json(OpenTerminalResponse {
                success: false,
                error: Some(format!("Failed to run osascript: {}", e)),
            })
        }
    }
}

/// Read the first meaningful content line from a markdown file
fn read_md_description(path: &Path) -> String {
    if let Ok(content) = std::fs::read_to_string(path) {
        content.lines()
            .map(|l| l.trim())
            .filter(|l| {
                !l.is_empty()
                    && !l.starts_with('#')
                    && !l.starts_with("![")
                    && !l.starts_with("---")
                    && !l.starts_with('<')
                    && !l.starts_with("[!")
                    && !l.starts_with("- ")
            })
            .next()
            .map(|s| {
                let s = s.strip_prefix('>').map(|r| r.trim()).unwrap_or(s);
                s.to_string()
            })
            .unwrap_or_else(|| "No description".to_string())
    } else {
        "No description".to_string()
    }
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
        eprintln!("Warning: Obsidian projects folder not found: {}", projects_path.display());
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
            let md_file = std::fs::read_dir(&entry_path).ok()
                .and_then(|entries| {
                    entries.flatten()
                        .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
                        .next()
                });
            let description = md_file.as_ref()
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
            let obsidian_url = format!("obsidian://open?vault={}&file={}", percent_encode(vault_name), percent_encode(&relative));

            let last_modified = entry.metadata().ok()
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
                            if idea.is_empty() { continue; }
                            let relative = format!("{}/{}", folder, "@Projects");
                            let obsidian_url = format!("obsidian://open?vault={}&file={}", percent_encode(vault_name), percent_encode(&relative));
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
                let obsidian_url = format!("obsidian://open?vault={}&file={}", percent_encode(vault_name), percent_encode(&relative));

                let last_modified = entry.metadata().ok()
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
    name.to_lowercase()
        .replace('-', "")
        .replace('_', "")
        .replace(' ', "")
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
            let files: Vec<&str> = stdout.lines().collect();
            Json(serde_json::json!({ "files": files }))
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Json(serde_json::json!({ "error": stderr.to_string() }))
        }
        Err(e) => {
            Json(serde_json::json!({ "error": format!("{}", e) }))
        }
    }
}

/// Auto-tag projects based on name, description, and tech stack
fn auto_tag_projects(projects: &mut [Project]) {
    let tag_keywords: &[(&str, &[&str])] = &[
        ("ai", &["ai", "llm", "gpt", "claude", "codex", "agent", "neural", "ml", "machine learning", "openai", "anthropic", "pytorch", "tensorflow"]),
        ("web", &["website", "landing", "frontend", "react", "next", "svelte", "html", "css", "tailwind", "vue", "angular", "sveltekit"]),
        ("api", &["api", "rest", "graphql", "endpoint", "server", "express", "axum", "hono", "fastapi"]),
        ("cli", &["cli", "terminal", "command line", "command-line", "shell", "bash"]),
        ("devops", &["docker", "kubernetes", "k8s", "ci/cd", "deploy", "infra", "terraform", "ansible", "helm", "umbrel"]),
        ("mobile", &["ios", "android", "mobile", "swift", "kotlin", "react native", "flutter"]),
        ("data", &["database", "postgres", "sqlite", "redis", "mongo", "data", "analytics", "scraping", "crawler"]),
        ("blockchain", &["blockchain", "web3", "solana", "ethereum", "crypto", "token", "nft", "rwa", "smart contract"]),
        ("seo", &["seo", "search engine", "sitemap", "analytics", "tracking"]),
        ("auth", &["auth", "login", "oauth", "jwt", "session", "credential", "password"]),
        ("bot", &["bot", "telegram", "whatsapp", "discord", "slack", "chat", "messaging"]),
        ("automation", &["automat", "workflow", "cron", "schedule", "scrape", "crawl", "hook"]),
        ("game", &["game", "rpg", "player", "level", "score"]),
        ("docs", &["documentation", "readme", "wiki", "knowledge", "obsidian", "note"]),
        ("finance", &["tax", "invoice", "payment", "billing", "finance", "accounting", "portfolio"]),
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
    let stopwords = ["the", "and", "for", "with", "from", "that", "this", "are", "was", "not",
        "you", "your", "has", "have", "had", "but", "all", "can", "will", "one", "our",
        "out", "use", "how", "its", "let", "may", "who", "did", "get", "she", "her",
        "him", "his", "old", "new", "now", "way", "each", "make", "like", "just",
        "over", "such", "take", "than", "them", "very", "when", "what", "some", "into",
        "been", "more", "other", "which", "about", "would", "their", "these", "could",
        "project", "using", "based", "built", "also", "here", "https", "http", "www",
        "com", "org", "github", "description", "provided", "none", "file", "code"];
    let lower = text.to_lowercase();
    lower.split(|c: char| !c.is_alphanumeric())
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
    let keywords: Vec<Vec<String>> = projects.iter().map(|p| {
        // Cap description to 200 chars to avoid noisy long descriptions
        let desc_cap: String = p.description.chars().take(200).collect();
        domain_keywords(&format!("{} {}", p.name, desc_cap))
    }).collect();

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
                reasons.push(format!("{} mentioned in {}", projects[i].name, projects[j].name));
            }
            if nj.len() >= 4 && di.contains(&nj) {
                weight += 6.0;
                reasons.push(format!("{} mentioned in {}", projects[j].name, projects[i].name));
            }

            // 3. Shared domain keywords from descriptions (goal similarity)
            let shared_kw: Vec<&String> = keywords[i].iter()
                .filter(|kw| keywords[j].contains(kw))
                .collect();
            let kw_score = (shared_kw.len() as f32).min(5.0);
            if kw_score >= 3.0 {
                weight += kw_score;
                reasons.push(format!("shared: {}", shared_kw.iter().take(3).map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            }

            // 4. Shared tags — only count if there's already another signal
            //    (prevents generic tag-only connections like "both tagged ai")
            let shared_tags: Vec<&String> = projects[i].tags.iter()
                .filter(|t| projects[j].tags.contains(t))
                .collect();
            if !shared_tags.is_empty() && weight > 0.0 {
                weight += shared_tags.len() as f32;
                reasons.push(format!("tags: {}", shared_tags.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            }

            // 5. Obsidian idea linked to implementation
            if (matches!(projects[i].project_type, ProjectType::Obsidian) && !matches!(projects[j].project_type, ProjectType::Obsidian))
                || (!matches!(projects[i].project_type, ProjectType::Obsidian) && matches!(projects[j].project_type, ProjectType::Obsidian))
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
    jobs: Arc<Mutex<Vec<AgentJob>>>,
    map_file: PathBuf,
}

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

fn default_model() -> String { "sonnet".to_string() }
fn default_permission_mode() -> String { "acceptEdits".to_string() }
fn default_budget() -> f64 { 1.0 }

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
        started_at: chrono::Local::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
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
    let job_id_clone = job_id.clone();
    let project_path = req.project_path.clone();
    let prompt = req.prompt.clone();
    let model = req.model;
    let permission_mode = req.permission_mode;
    let max_budget = req.max_budget_usd;

    tokio::spawn(async move {
        let result = run_swarm_task(
            &job_id_clone, &project_path, &prompt,
            &model, &permission_mode, max_budget,
        ).await;

        let mut jobs = jobs.lock().await;
        if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id_clone) {
            match result {
                Ok(outcome) => {
                    job.status = "succeeded".to_string();
                    job.cost_usd = outcome.cost_usd;
                    job.tool_calls = outcome.tool_calls;
                    job.input_tokens = outcome.input_tokens;
                    job.output_tokens = outcome.output_tokens;
                    job.summary = outcome.summary;
                    job.branch = outcome.branch;
                    job.changed_files = outcome.changed_files;
                }
                Err(e) => {
                    job.status = "failed".to_string();
                    job.error = Some(e);
                }
            }
            job.finished_at = Some(chrono::Local::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
    });

    Json(serde_json::json!({ "job_id": job_id }))
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
    job_id: &str, project_path: &str, prompt: &str,
    model: &str, permission_mode: &str, max_budget: f64,
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
            retry_policy: RetryPolicy { max_retries: 0, retry_on_timeout: false, retry_on_cli_error: false },
            failure_policy: FailurePolicy::SkipDependents,
            cleanup_policy: CleanupPolicy::OnSuccess,
            timeout_action: TimeoutAction::FailImmediately,
            budget_action: BudgetAction::CancelTask,
        },
        execution: TaskExecutionConfig {
            allowed_tools: vec![
                "Read".into(), "Edit".into(), "Write".into(),
                "Bash".into(), "Glob".into(), "Grep".into(),
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
        .and_then(|o| if o.status.success() {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else { None })
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
    let snapshot = orchestrator.run_graph(graph).await.map_err(|e| format!("{}", e))?;

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

async fn agent_jobs(
    State(state): State<AppState>,
) -> Json<Vec<AgentJob>> {
    let jobs = state.jobs.lock().await;
    Json(jobs.clone())
}

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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Survey { path, output, github, gitlab, watch, obsidian, obsidian_folder, obsidian_vault, obsidian_sync } => {
            loop {
                eprintln!("Surveying {}...", path.display());

                let mut all_projects = survey_projects(&path);
                let local_count = all_projects.len();

                if let Some(gh_user) = &github {
                    eprintln!("Fetching GitHub repos for {}...", gh_user);
                    let gh_repos = fetch_github_repos(gh_user).await;
                    all_projects.extend(gh_repos);
                }

                if let Some(gl_user) = &gitlab {
                    eprintln!("Fetching GitLab repos for {}...", gl_user);
                    let gl_repos = fetch_gitlab_repos(gl_user).await;
                    all_projects.extend(gl_repos);
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
                            Ok(o) => eprintln!("Warning: ob sync failed: {}", String::from_utf8_lossy(&o.stderr)),
                            Err(_) => eprintln!("Warning: `ob` command not found. Skipping sync."),
                        }
                    }
                    let vault_name = obsidian_vault.as_deref()
                        .or_else(|| vault_path.file_name().and_then(|n| n.to_str()))
                        .unwrap_or("vault");
                    eprintln!("Scanning Obsidian vault '{}' at {}...", vault_name, vault_path.display());
                    let obs_projects = scan_obsidian_vault(vault_path, &obsidian_folder, vault_name);
                    eprintln!("Found {} Obsidian notes/ideas", obs_projects.len());
                    all_projects.extend(obs_projects);
                }

                let before_dedup = all_projects.len();
                let all_projects = deduplicate_projects(all_projects);
                let mut all_projects = link_obsidian_notes(all_projects);
                let merged = before_dedup - all_projects.len();

                // Auto-tag all projects
                auto_tag_projects(&mut all_projects);

                if let Err(e) = save_map(&all_projects, &output) {
                    eprintln!("Error: {}", e);
                    if watch.is_none() { std::process::exit(1); }
                } else {
                    let github_count = all_projects.iter().filter(|p| matches!(p.project_type, ProjectType::GitHub)).count();
                    let dirty_count = all_projects.iter().filter(|p| p.git_status.as_deref() == Some("uncommitted")).count();

                    println!("[{}] {} projects ({} local, {} github, {} merged, {} dirty) -> {}",
                        chrono::Local::now().format("%H:%M:%S"),
                        all_projects.len(), local_count, github_count, merged, dirty_count,
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
        Commands::Serve { port, bind, map_file } => {
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
                jobs: Arc::new(Mutex::new(Vec::new())),
                map_file: map_file.clone(),
            };

            let app = Router::new()
                .route("/api/map", get(serve_map))
                .route("/api/graph", get(serve_graph))
                .route("/api/open-terminal", post(open_terminal))
                .route("/api/git-status", get(get_git_status_api))
                .route("/api/agent/run", post(agent_run))
                .route("/api/agent/jobs", get(agent_jobs))
                .route("/api/agent/job/{id}", get(agent_job_detail))
                .route("/api/agent/job/{id}/log", get(agent_job_log))
                .with_state(app_state)
                .fallback_service(ServeDir::new("dist"));

            let addr = SocketAddr::from((bind, port));
            println!("Mercator map available at http://{}", addr);
            println!("Press Ctrl+C to stop");
            
            let listener = tokio::net::TcpListener::bind(addr).await
                .expect("Failed to bind to port");
            axum::serve(listener, app).await
                .expect("Server error");
        }
    }
}
