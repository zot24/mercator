//! Core project data model + map persistence.
//!
//! `Project` is the unit every Mercator surface trades in. The Serde renames
//! match the JSON shape the dashboard expects (`techStack`, `gitBranch`, …),
//! so the on-disk format and the `/api/map` payload are byte-for-byte the
//! same value.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub description: String,
    pub project_type: ProjectType,
    #[serde(rename = "lastModified")]
    pub last_modified: Option<String>,
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,
    #[serde(rename = "lastCommit")]
    pub last_commit: Option<String>,
    #[serde(rename = "gitStatus")]
    pub git_status: Option<String>,
    #[serde(rename = "techStack")]
    pub tech_stack: Vec<String>,
    #[serde(rename = "remoteUrl")]
    pub remote_url: Option<String>,
    /// Detected AI agent used in this project (e.g., "claude", "codex")
    #[serde(rename = "agentUsed")]
    pub agent_used: Option<String>,
    /// Obsidian URI to open the linked note
    #[serde(
        rename = "obsidianUrl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub obsidian_url: Option<String>,
    /// Relative path to the Obsidian note within the vault
    #[serde(
        rename = "obsidianNotePath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub obsidian_note_path: Option<String>,
    /// Auto-generated topic tags for graph edges and semantic grouping
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ProjectType {
    Git,
    Folder,
    Idea,
    GitHub,
    GitLab,
    Obsidian,
}

/// Format a SystemTime as an ISO 8601 string in UTC.
pub fn format_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Serialize the project list as pretty JSON to `output`.
pub fn save_map(projects: &[Project], output: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(projects)
        .map_err(|e| format!("Failed to serialize projects: {}", e))?;
    std::fs::write(output, &json)
        .map_err(|e| format!("Failed to write to {}: {}", output.display(), e))?;
    Ok(())
}

/// Read the project map JSON back into memory.
pub fn load_map(path: &Path) -> Result<Vec<Project>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse JSON: {}", e))
}
