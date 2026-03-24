// Mercator - Project Topography Tool
// A Rust CLI tool for discovering and visualizing your local development projects

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::process::Command;
use walkdir::WalkDir;
use axum::{routing::{get, post}, Json, Router};
use std::net::{IpAddr, SocketAddr};
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
            let description = std::fs::read_dir(&entry_path).ok()
                .and_then(|entries| {
                    entries.flatten()
                        .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
                        .next()
                })
                .map(|md| read_md_description(&md.path()))
                .unwrap_or_else(|| "Obsidian project folder".to_string());

            let relative = format!("{}/{}", folder, name);
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
                            let relative = format!("{}/{}", folder, name);
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

/// Compute a relationship graph from projects
fn compute_graph(projects: &[Project]) -> serde_json::Value {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

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

    // Build edges: shared tags + shared tech
    for i in 0..projects.len() {
        for j in (i + 1)..projects.len() {
            let mut weight: f32 = 0.0;
            let mut shared = Vec::new();

            // Shared tags (strong signal)
            for tag in &projects[i].tags {
                if projects[j].tags.contains(tag) {
                    weight += 2.0;
                    shared.push(tag.clone());
                }
            }

            // Shared tech stack
            for tech in &projects[i].tech_stack {
                if projects[j].tech_stack.contains(tech) {
                    weight += 1.0;
                    if !shared.contains(tech) {
                        shared.push(tech.clone());
                    }
                }
            }

            // Same agent
            if let (Some(a), Some(b)) = (&projects[i].agent_used, &projects[j].agent_used) {
                if a == b {
                    weight += 0.5;
                }
            }

            // Obsidian link (explicit connection)
            if projects[i].obsidian_url.is_some() && projects[j].obsidian_url.is_some() {
                weight += 1.0;
            }

            // Only include edges with meaningful weight
            if weight >= 2.0 {
                edges.push(serde_json::json!({
                    "source": i,
                    "target": j,
                    "weight": weight,
                    "shared": shared,
                }));
            }
        }
    }

    serde_json::json!({ "nodes": nodes, "edges": edges })
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

            let app = Router::new()
                .route("/api/map", get(serve_map))
                .route("/api/graph", get(serve_graph))
                .route("/api/open-terminal", post(open_terminal))
                .route("/api/git-status", get(get_git_status_api))
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
