//! Render a Markdown "projects" section for a profile README and splice it into
//! a file between stable markers. Powers the `mercator readme` command.
//!
//! The rendering helpers here are pure (no DB, no clock): the caller pulls the
//! projects from the DB (the active set by default) and passes the timestamp in,
//! so the output is deterministic and unit-testable. Only [`inject_file`] touches
//! the filesystem.

use crate::project::{Project, ProjectType};
use chrono::{DateTime, Utc};

/// Markers that delimit the generated block in the target README. Everything
/// between them is owned by mercator and replaced on each run; everything
/// outside is left untouched.
pub const START_MARKER: &str = "<!-- MERCATOR:START -->";
pub const END_MARKER: &str = "<!-- MERCATOR:END -->";

/// Layout for the rendered project list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// A Markdown table: `Project | Description | Stack`.
    Table,
    /// A bullet list — `- <emoji> **[name](url)** — description · ` + tech — that
    /// reads well in a profile README's "currently building" section.
    List,
}

/// Knobs for [`render_block`].
pub struct ReadmeOptions {
    pub title: String,
    pub badge: bool,
    pub limit: Option<usize>,
    pub layout: Layout,
    /// Prefix each project with a tech-derived emoji (list layout only).
    pub emoji: bool,
}

impl Default for ReadmeOptions {
    fn default() -> Self {
        Self {
            title: "🛠️ What I'm working on".to_string(),
            badge: true,
            limit: None,
            layout: Layout::Table,
            emoji: true,
        }
    }
}

/// Convert a git remote (scp-style ssh, `ssh://`, or http(s), with or without a
/// trailing `.git`) into a browsable `https://` URL. Returns `None` for empty or
/// unrecognised remotes (e.g. local paths) so the caller can fall back to plain
/// text.
pub fn web_url(remote: &str) -> Option<String> {
    let r = remote.trim();
    if r.is_empty() {
        return None;
    }
    let normalized = if let Some(rest) = r.strip_prefix("git@") {
        // scp-like: git@host:owner/repo(.git) -> https://host/owner/repo
        let (host, path) = rest.split_once(':')?;
        format!("https://{}/{}", host, path)
    } else if let Some(rest) = r.strip_prefix("ssh://git@") {
        format!("https://{}", rest)
    } else if r.starts_with("https://") || r.starts_with("http://") {
        r.to_string()
    } else {
        return None;
    };
    let url = normalized.trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    Some(url.to_string())
}

/// Escape a value for a single Markdown table cell: collapse all whitespace
/// (including newlines) to single spaces and escape the column separator.
fn cell(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Truncate to at most `max` characters (not bytes), appending an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// A non-empty display name: the project's `name`, else the repo name from its
/// remote, else the last meaningful path segment, else `"project"`.
fn display_name(p: &Project) -> String {
    let n = p.name.trim();
    if !n.is_empty() {
        return n.to_string();
    }
    if let Some(url) = p.remote_url.as_deref().and_then(web_url) {
        if let Some(seg) = url.rsplit('/').find(|s| !s.is_empty()) {
            return seg.to_string();
        }
    }
    p.path
        .rsplit('/')
        .find(|s| !s.is_empty() && *s != ".")
        .unwrap_or("project")
        .to_string()
}

fn name_cell(p: &Project) -> String {
    let name = cell(&display_name(p));
    match p.remote_url.as_deref().and_then(web_url) {
        Some(url) => format!("[{}]({})", name, url),
        None => name,
    }
}

fn stack_cell(p: &Project) -> String {
    if p.tech_stack.is_empty() {
        "—".to_string()
    } else {
        p.tech_stack
            .iter()
            .map(|t| format!("`{}`", cell(t)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A generic, tech-derived emoji for a project. Prefers a **language** glyph
/// (the most identifying signal) over tooling like Docker, scanning the whole
/// tech stack rather than just the first entry — so a `[Docker, Rust]` project
/// reads as 🦀, not 🐳 — then falls back to the project type. Deliberately
/// agnostic: no per-project or per-user hard-coding.
pub fn tech_emoji(p: &Project) -> &'static str {
    fn language(tech: &str) -> &'static str {
        match tech {
            "rust" => "🦀",
            "go" | "golang" => "🐹",
            "python" => "🐍",
            "swift" => "🍎",
            "typescript" => "🔷",
            "javascript" | "node.js" | "node" | "bun" | "deno" => "🟢",
            "ruby" => "💎",
            "php" => "🐘",
            "java" | "kotlin" => "☕",
            "c" | "c++" | "cpp" => "⚙️",
            "c#" | "csharp" | ".net" => "🟣",
            "elixir" => "💧",
            "zig" => "⚡",
            "shell" | "bash" => "🐚",
            "html" | "css" => "🌐",
            _ => "",
        }
    }
    fn tooling(tech: &str) -> &'static str {
        match tech {
            "docker" => "🐳",
            "kubernetes" | "k8s" => "☸️",
            "terraform" => "🏗️",
            _ => "",
        }
    }
    // Pass 1: first known language. Pass 2: first known tool. Then type.
    for t in &p.tech_stack {
        let g = language(&t.to_lowercase());
        if !g.is_empty() {
            return g;
        }
    }
    for t in &p.tech_stack {
        let g = tooling(&t.to_lowercase());
        if !g.is_empty() {
            return g;
        }
    }
    match p.project_type {
        ProjectType::Idea => "💡",
        ProjectType::Folder => "📁",
        ProjectType::Obsidian => "📓",
        _ => "📦",
    }
}

/// One bullet for the list layout: `- 🦀 **[name](url)** — description · `Rust``.
fn list_item(p: &Project, emoji: bool) -> String {
    let mut s = String::from("- ");
    if emoji {
        let e = tech_emoji(p);
        if !e.is_empty() {
            s.push_str(e);
            s.push(' ');
        }
    }
    s.push_str(&format!("**{}**", name_cell(p)));
    let desc = truncate(&cell(&p.description), 160);
    if !desc.is_empty() {
        s.push_str(" — ");
        s.push_str(&desc);
    }
    if !p.tech_stack.is_empty() {
        s.push_str(" · ");
        s.push_str(&stack_cell(p));
    }
    s
}

fn badge_line(generated: DateTime<Utc>) -> String {
    format!(
        "<sub>[![mapped by zot24/mercator]\
         (https://img.shields.io/badge/mapped_by-zot24%2Fmercator-2563eb?style=flat-square&logo=rust&logoColor=white)]\
         (https://github.com/zot24/mercator) · updated {}</sub>",
        generated.format("%Y-%m-%d")
    )
}

/// Render the full Markdown block — markers included — for `projects`. Projects
/// are sorted by name for a stable, low-churn diff, then capped to
/// `opts.limit`.
pub fn render_block(
    projects: &[Project],
    opts: &ReadmeOptions,
    generated: DateTime<Utc>,
) -> String {
    let mut shown: Vec<&Project> = projects.iter().collect();
    shown.sort_by_key(|p| display_name(p).to_lowercase());
    if let Some(n) = opts.limit {
        shown.truncate(n);
    }

    let mut lines: Vec<String> = vec![
        START_MARKER.to_string(),
        "<!-- Generated by `mercator readme` — do not edit this section by hand. -->".to_string(),
        String::new(),
        format!("## {}", opts.title),
        String::new(),
    ];

    if shown.is_empty() {
        lines.push(
            "_No active projects yet — add one with `mercator active add <path>`._".to_string(),
        );
    } else {
        match opts.layout {
            Layout::Table => {
                lines.push("| Project | Description | Stack |".to_string());
                lines.push("| --- | --- | --- |".to_string());
                for p in &shown {
                    let desc = truncate(&cell(&p.description), 110);
                    let desc = if desc.is_empty() {
                        "—".to_string()
                    } else {
                        desc
                    };
                    lines.push(format!(
                        "| {} | {} | {} |",
                        name_cell(p),
                        desc,
                        stack_cell(p)
                    ));
                }
            }
            Layout::List => {
                for p in &shown {
                    lines.push(list_item(p, opts.emoji));
                }
            }
        }
    }

    if opts.badge {
        lines.push(String::new());
        lines.push(badge_line(generated));
    }
    lines.push(END_MARKER.to_string());
    lines.join("\n")
}

/// Splice `block` (which already carries the markers) into `content`. If both
/// markers are present, the span from the first `START_MARKER` to the first
/// following `END_MARKER` is replaced. Otherwise the block is appended with a
/// blank-line separator. Idempotent: re-running replaces the previous block.
pub fn inject(content: &str, block: &str) -> String {
    let block = block.trim_end();
    if let Some(start) = content.find(START_MARKER) {
        let after = start + START_MARKER.len();
        if let Some(rel) = content[after..].find(END_MARKER) {
            let end = after + rel + END_MARKER.len();
            let mut out = String::with_capacity(content.len() + block.len());
            out.push_str(&content[..start]);
            out.push_str(block);
            out.push_str(&content[end..]);
            return out;
        }
    }
    let mut out = content.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    out
}

/// Read `path` (a missing file is treated as empty), splice `block` in, and write
/// it back. Creates the file if it didn't exist.
pub fn inject_file(path: &std::path::Path, block: &str) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {}", path.display(), e)),
    };
    let updated = inject(&existing, block);
    std::fs::write(path, updated).map_err(|e| format!("write {}: {}", path.display(), e))
}

/// Keep only projects whose remote is a reachable **public** repo. Does an
/// unauthenticated GET of each unique web URL — a public repo answers 2xx, a
/// private or missing one answers 404 — so it needs no token and no schema
/// change. Projects without a browsable remote are dropped (we can't verify
/// them as public, and a profile README should never risk leaking private work).
/// Returns `(kept, dropped_count)`.
pub async fn retain_public(projects: Vec<Project>) -> (Vec<Project>, usize) {
    use std::collections::HashMap;
    let client = match reqwest::Client::builder()
        .user_agent("mercator-readme")
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => return (projects, 0),
    };
    let mut cache: HashMap<String, bool> = HashMap::new();
    let mut kept = Vec::new();
    let mut dropped = 0;
    for p in projects {
        let public = match p.remote_url.as_deref().and_then(web_url) {
            Some(url) => match cache.get(&url) {
                Some(&v) => v,
                None => {
                    let v = client
                        .get(&url)
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    cache.insert(url, v);
                    v
                }
            },
            None => false,
        };
        if public {
            kept.push(p);
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectType;

    fn project(name: &str, desc: &str, remote: Option<&str>, tech: &[&str]) -> Project {
        Project {
            name: name.into(),
            path: format!("/code/{}", name),
            description: desc.into(),
            project_type: ProjectType::GitHub,
            last_modified: None,
            git_branch: None,
            last_commit: None,
            git_status: None,
            ahead: None,
            behind: None,
            tech_stack: tech.iter().map(|s| s.to_string()).collect(),
            remote_url: remote.map(|s| s.to_string()),
            agent_used: None,
            obsidian_url: None,
            obsidian_note_path: None,
            tags: vec![],
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn web_url_handles_ssh_https_and_dotgit() {
        assert_eq!(
            web_url("git@github.com:zot24/dewey.git").as_deref(),
            Some("https://github.com/zot24/dewey")
        );
        assert_eq!(
            web_url("https://gitlab.com/zot24/x.git/").as_deref(),
            Some("https://gitlab.com/zot24/x")
        );
        assert_eq!(
            web_url("ssh://git@github.com/a/b.git").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(
            web_url("https://github.com/a/b").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(web_url(""), None);
        assert_eq!(web_url("/local/path"), None);
    }

    #[test]
    fn display_name_falls_back_for_empty_name() {
        // empty name → repo name from remote
        let p = project("", "d", Some("git@github.com:zot24/mercator.git"), &[]);
        assert_eq!(display_name(&p), "mercator");
        // empty name + no remote → last path segment
        let mut q = project("", "d", None, &[]);
        q.path = "/code/skylog".into();
        assert_eq!(display_name(&q), "skylog");
    }

    #[test]
    fn cell_escapes_pipes_and_collapses_whitespace() {
        assert_eq!(cell("a | b\nc"), "a \\| b c");
    }

    #[test]
    fn tech_emoji_maps_language_then_type() {
        assert_eq!(tech_emoji(&project("a", "", None, &["Rust"])), "🦀");
        assert_eq!(tech_emoji(&project("a", "", None, &["Node.js"])), "🟢");
        // prefers a language glyph over tooling, regardless of stack order
        assert_eq!(
            tech_emoji(&project("a", "", None, &["Docker", "Rust"])),
            "🦀"
        );
        // tooling glyph when no language is present
        assert_eq!(tech_emoji(&project("a", "", None, &["Docker"])), "🐳");
        // unknown tech falls back to type glyph
        assert_eq!(tech_emoji(&project("a", "", None, &["Cobol"])), "📦");
        // no tech → type fallback
        let mut idea = project("a", "", None, &[]);
        idea.project_type = ProjectType::Idea;
        assert_eq!(tech_emoji(&idea), "💡");
    }

    #[test]
    fn list_layout_renders_emoji_bullet_with_bold_link() {
        let projects = vec![project(
            "zskills",
            "Declarative package manager for agentic CLIs.",
            Some("git@github.com:zot24/zskills.git"),
            &["Rust"],
        )];
        let opts = ReadmeOptions {
            layout: Layout::List,
            badge: false,
            ..Default::default()
        };
        let out = render_block(&projects, &opts, at("2026-06-21T00:00:00Z"));
        assert!(out.contains("- 🦀 **[zskills](https://github.com/zot24/zskills)** —"));
        assert!(out.contains("`Rust`"));
        assert!(!out.contains("| Project |")); // no table in list mode
    }

    #[test]
    fn list_layout_no_emoji_option() {
        let projects = vec![project("x", "d", None, &["Rust"])];
        let opts = ReadmeOptions {
            layout: Layout::List,
            emoji: false,
            ..Default::default()
        };
        let out = render_block(&projects, &opts, at("2026-06-21T00:00:00Z"));
        assert!(out.contains("- **x**"));
        assert!(!out.contains("🦀"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
    }

    #[test]
    fn render_block_has_markers_table_link_and_badge() {
        let projects = vec![project(
            "dewey",
            "Your wiki's librarian, on the desktop.",
            Some("git@github.com:zot24/dewey.git"),
            &["Swift"],
        )];
        let opts = ReadmeOptions::default();
        let out = render_block(&projects, &opts, at("2026-06-21T00:00:00Z"));

        assert!(out.starts_with(START_MARKER));
        assert!(out.trim_end().ends_with(END_MARKER));
        assert!(out.contains("## 🛠️ What I'm working on"));
        assert!(out.contains("[dewey](https://github.com/zot24/dewey)"));
        assert!(out.contains("`Swift`"));
        assert!(out.contains("mercator")); // badge
        assert!(out.contains("updated 2026-06-21"));
    }

    #[test]
    fn render_block_respects_limit_and_sorts_by_name() {
        let projects = vec![
            project("zebra", "z", None, &[]),
            project("alpha", "a", None, &[]),
            project("mango", "m", None, &[]),
        ];
        let opts = ReadmeOptions {
            limit: Some(2),
            ..Default::default()
        };
        let out = render_block(&projects, &opts, at("2026-06-21T00:00:00Z"));
        assert!(out.contains("alpha"));
        assert!(out.contains("mango"));
        assert!(!out.contains("zebra")); // truncated after sort
    }

    #[test]
    fn render_block_empty_is_friendly() {
        let out = render_block(&[], &ReadmeOptions::default(), at("2026-06-21T00:00:00Z"));
        assert!(out.contains("No active projects"));
        assert!(out.starts_with(START_MARKER));
    }

    #[test]
    fn no_badge_option_omits_badge() {
        let opts = ReadmeOptions {
            badge: false,
            ..Default::default()
        };
        let out = render_block(&[], &opts, at("2026-06-21T00:00:00Z"));
        assert!(!out.contains("shields.io"));
    }

    #[test]
    fn inject_replaces_between_markers() {
        let content = format!(
            "# Me\n\nintro\n\n{}\nOLD\n{}\n\nfooter\n",
            START_MARKER, END_MARKER
        );
        let block = format!("{}\nNEW\n{}", START_MARKER, END_MARKER);
        let out = inject(&content, &block);
        assert!(out.contains("NEW"));
        assert!(!out.contains("OLD"));
        assert!(out.contains("# Me"));
        assert!(out.contains("footer"));
    }

    #[test]
    fn inject_appends_when_markers_absent() {
        let content = "# Profile\n\nhi";
        let block = format!("{}\nBODY\n{}", START_MARKER, END_MARKER);
        let out = inject(content, &block);
        assert!(out.starts_with("# Profile"));
        assert!(out.contains(START_MARKER));
        assert!(out.trim_end().ends_with(END_MARKER));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn inject_is_idempotent() {
        let block1 = format!("{}\nONE\n{}", START_MARKER, END_MARKER);
        let block2 = format!("{}\nTWO\n{}", START_MARKER, END_MARKER);
        let once = inject("# x\n", &block1);
        let twice = inject(&once, &block2);
        assert!(twice.contains("TWO"));
        assert!(!twice.contains("ONE"));
        // Only one marker pair remains.
        assert_eq!(twice.matches(START_MARKER).count(), 1);
    }

    #[test]
    fn inject_file_creates_and_updates() {
        let dir = std::env::temp_dir().join(format!("mercator-readme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("README.md");

        let block = format!("{}\nBODY1\n{}", START_MARKER, END_MARKER);
        inject_file(&target, &block).unwrap();
        assert!(std::fs::read_to_string(&target).unwrap().contains("BODY1"));

        let block2 = format!("{}\nBODY2\n{}", START_MARKER, END_MARKER);
        inject_file(&target, &block2).unwrap();
        let updated = std::fs::read_to_string(&target).unwrap();
        assert!(updated.contains("BODY2"));
        assert_eq!(updated.matches(START_MARKER).count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
