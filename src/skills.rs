//! Skills inventory — walks `~/.claude/skills/`, the plugin marketplace
//! cache, and every local project's `.claude/skills/` to build a grouped
//! view with drift detection.
//!
//! Output shape is `Vec<SkillGroup>`. Plugin-sourced skills group by
//! marketplace (`zot24-skills`, `claude-plugins-official`, …); standalone
//! skills group by name prefix when ≥2 share one (`gsd-*`); the rest fall
//! into `core`.
//!
//! Drift between project copies and the global copy is computed with a
//! cheap content hash (FNV-on-Rust-DefaultHasher); status is one of
//! `synced`, `diverged`, or `no-global`.

use crate::project::{load_map, ProjectType};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct SkillUsage {
    pub project: String,
    pub path: String,
    pub skill_path: String,
    pub hash: u64,
    /// Status compared to the global copy: "synced", "diverged", or "no-global"
    pub status: String,
}

#[derive(Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Frontmatter `version` if present, otherwise plugin version from path
    pub version: Option<String>,
    /// True if a global copy exists at ~/.claude/skills/<name>/
    pub has_global: bool,
    pub global_hash: Option<u64>,
    pub global_path: Option<String>,
    pub projects: Vec<SkillUsage>,
    /// Marketplace name for plugin skills, name prefix otherwise
    pub group: String,
    /// Source repo URL (from frontmatter or marketplace manifest)
    pub repo_url: Option<String>,
}

#[derive(Serialize)]
pub struct SkillGroup {
    pub name: String,
    pub repo_url: Option<String>,
    pub skills: Vec<SkillEntry>,
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
        let Ok(plugins) = std::fs::read_dir(&mp_path) else {
            continue;
        };
        for plugin_entry in plugins.flatten() {
            let plugin_path = plugin_entry.path();
            if !plugin_path.is_dir() {
                continue;
            }
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

/// Derive a group name from a skill name. `plugin:skill` → `plugin`;
/// otherwise the first hyphen segment if ≥2 chars; else `core`.
pub fn name_prefix_group(name: &str) -> String {
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

/// Build the grouped skill inventory. Pure-ish (touches the filesystem,
/// no HTTP/no global state). Wrap in a route handler that calls this and
/// `Json(...)`s the result.
pub fn compute_skill_groups(map_file: &Path) -> Vec<SkillGroup> {
    use std::collections::HashMap;

    let home = dirs::home_dir().unwrap_or_default();
    let marketplace_repos = load_marketplace_repos(&home);
    let mut entries: HashMap<String, SkillEntry> = HashMap::new();

    // 1. Global skills
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
                group: String::new(),
                repo_url: sr.repository,
            },
        );
    }

    // 2. Plugin skills (marketplace cache)
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
    let projects = load_map(map_file).unwrap_or_default();
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

    // 4. Assign groups: name-prefix grouping iff ≥2 share the prefix
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

    // 5. Bucket
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
        let a_core = a.name == "core";
        let b_core = b.name == "core";
        a_core
            .cmp(&b_core)
            .then_with(|| b.skills.len().cmp(&a.skills.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}
