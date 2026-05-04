//! Project sources: filesystem survey, GitHub / GitLab fetchers, Obsidian
//! vault scan, deduplication, and the small parsers / detectors that feed
//! them. Everything here turns a "where to look" into a `Vec<Project>`.

use crate::markdown::{description_from_repo, percent_encode, read_md_description};
use crate::project::{format_time, Project, ProjectType};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;

// ── git helpers ────────────────────────────────────────────────────────

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

// ── HTTP error formatting + Link header parsing ────────────────────────

/// Format an HTTP error response into a single-line summary suitable for
/// stderr. Pulls the JSON `message` field if present (GitHub / GitLab both
/// use it for 4xx errors) and includes any rate-limit headers GitHub
/// returns. Pure function so it can be unit-tested without HTTP.
pub fn format_api_error(
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
pub fn parse_link_next(link_header: &str) -> Option<String> {
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

// ── GitHub ─────────────────────────────────────────────────────────────

/// Fetch repositories from GitHub API. Paginates via `Link: rel="next"` and
/// authenticates with a token if provided.
pub async fn fetch_github_repos(
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
pub struct GitHubRepo {
    pub name: String,
    pub description: Option<String>,
    pub html_url: String,
    pub pushed_at: String,
    pub default_branch: Option<String>,
    pub language: Option<String>,
    pub topics: Option<Vec<String>>,
}

/// Detect tech stack from GitHub repo metadata
pub fn detect_github_tech_stack(repo: &GitHubRepo) -> Vec<String> {
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

// ── GitLab ─────────────────────────────────────────────────────────────

/// Fetch repositories from GitLab API. Paginates via `x-next-page` header
/// and authenticates with a token via the `PRIVATE-TOKEN` header.
pub async fn fetch_gitlab_repos(
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
pub struct GitLabRepo {
    pub name: String,
    pub description: Option<String>,
    pub web_url: String,
    pub last_activity_at: String,
    pub default_branch: Option<String>,
    #[serde(rename = "tag_list")]
    pub tag_list: Option<Vec<String>>,
}

/// Detect tech stack from GitLab repo metadata
pub fn detect_gitlab_tech_stack(repo: &GitLabRepo) -> Vec<String> {
    let mut stack = Vec::new();

    if let Some(tags) = &repo.tag_list {
        for tag in tags.iter().take(3) {
            stack.push(tag.clone());
        }
    }

    stack
}

// ── Local filesystem survey ────────────────────────────────────────────

/// Detect the tech stack of a project by looking for known marker files
pub fn detect_tech_stack(path: &Path) -> Vec<String> {
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
pub fn detect_agent(path: &Path) -> Option<String> {
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

pub fn survey_projects(root: &Path) -> Vec<Project> {
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

// ── Dedup ──────────────────────────────────────────────────────────────

/// Normalize a remote URL for comparison (strip .git suffix, protocol, trailing slashes)
pub fn normalize_remote_url(url: &str) -> String {
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
pub fn deduplicate_projects(projects: Vec<Project>) -> Vec<Project> {
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

// ── Obsidian ───────────────────────────────────────────────────────────

/// Scan an Obsidian vault's Projects folder for idea/project notes
pub fn scan_obsidian_vault(vault_path: &Path, folder: &str, vault_name: &str) -> Vec<Project> {
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
pub fn normalize_name(name: &str) -> String {
    name.to_lowercase().replace(['-', '_', ' '], "")
}

/// Link Obsidian notes to existing projects by name matching.
/// Matched Obsidian entries merge their obsidian_url into the existing project and are removed.
pub fn link_obsidian_notes(projects: Vec<Project>) -> Vec<Project> {
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
