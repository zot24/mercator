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

/// Serialize the project list as pretty JSON to `output`. Atomic on POSIX:
/// writes to a sibling `.tmp` file then `rename(2)`s over the target so a
/// reader (e.g. `/api/map` while `survey --watch` is running) never sees a
/// half-written file. The OS guarantees rename appears as a single inode
/// swap from the reader's perspective.
pub fn save_map(projects: &[Project], output: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(projects)
        .map_err(|e| format!("Failed to serialize projects: {}", e))?;
    let tmp = tmp_path_for(output);
    std::fs::write(&tmp, &json)
        .map_err(|e| format!("Failed to write tmp {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, output).map_err(|e| {
        // Best effort cleanup if rename failed
        let _ = std::fs::remove_file(&tmp);
        format!(
            "Failed to rename {} → {}: {}",
            tmp.display(),
            output.display(),
            e
        )
    })?;
    Ok(())
}

/// Compute a sibling tempfile path next to `target`. Pure function so the
/// rename invariant can be unit-tested without touching the filesystem.
pub fn tmp_path_for(target: &Path) -> std::path::PathBuf {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mercator_map.json".to_string());
    parent.join(format!(".{}.tmp", name))
}

/// Read the project map JSON back into memory.
pub fn load_map(path: &Path) -> Result<Vec<Project>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse JSON: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tmp_path_keeps_sibling_directory() {
        assert_eq!(
            tmp_path_for(&PathBuf::from("/tmp/foo/mercator_map.json")),
            PathBuf::from("/tmp/foo/.mercator_map.json.tmp")
        );
    }

    #[test]
    fn tmp_path_handles_bare_filename() {
        assert_eq!(
            tmp_path_for(&PathBuf::from("mercator_map.json")),
            PathBuf::from("./.mercator_map.json.tmp")
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("mercator-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("map.json");

        let p = Project {
            name: "x".into(),
            path: "/x".into(),
            description: String::new(),
            project_type: ProjectType::Git,
            last_modified: None,
            git_branch: None,
            last_commit: None,
            git_status: None,
            tech_stack: vec![],
            remote_url: None,
            agent_used: None,
            obsidian_url: None,
            obsidian_note_path: None,
            tags: vec![],
        };
        save_map(&[p], &target).unwrap();
        let back = load_map(&target).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "x");
        // The .tmp sibling should not survive the rename
        assert!(!tmp_path_for(&target).exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
