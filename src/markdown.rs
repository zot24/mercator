//! Markdown helpers — description extraction, YAML frontmatter quoting, and
//! the `mercator export` rendering.
//!
//! Everything here is pure (or filesystem-read-only); no HTTP, no global
//! state. The functions are unit-tested from `main.rs` so the test bodies
//! stay near where the assertions are written; only the implementations live
//! here.

use crate::project::Project;
use std::path::Path;

/// Strip simple inline markdown: `[text](url)` → `text`, `**x**` → `x`,
/// `\`x\`` → `x`. Anything more elaborate (HTML, footnotes) survives.
pub fn strip_inline_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Iterate by char (not byte) so multi-byte UTF-8 — em-dashes, emoji,
    // accents — survives intact. `char_indices` still yields byte offsets, so
    // the link-slicing arithmetic below is unchanged.
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '[' {
            if let Some(close_brk) = s[i..].find("](") {
                let text_end = i + close_brk;
                if let Some(close_par) = s[text_end + 2..].find(')') {
                    out.push_str(&s[i + 1..text_end]);
                    // Skip the iterator past the consumed `[text](url)`.
                    let target = text_end + 2 + close_par + 1;
                    while chars.peek().is_some_and(|&(j, _)| j < target) {
                        chars.next();
                    }
                    continue;
                }
            }
            out.push(c);
        } else if c == '*' || c == '_' {
            // Drop a run of the same emphasis marker.
            while chars.peek().is_some_and(|&(_, nc)| nc == c) {
                chars.next();
            }
        } else if c == '`' {
            // Drop the backtick.
        } else {
            out.push(c);
        }
    }
    out
}

/// Extract a clean description paragraph from a markdown file. Skips YAML
/// frontmatter, headings, badges, callouts, and reference link defs;
/// returns the first prose paragraph (joined to a single line) capped at
/// ~240 chars on a word boundary.
pub fn extract_md_description(path: &Path) -> Option<String> {
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

    let mut paragraph = String::new();
    let mut started = false;
    for line in lines.iter() {
        let t = line.trim();
        if !started {
            if is_skip(t) {
                continue;
            }
            started = true;
        } else if t.is_empty() || is_skip(t) {
            break;
        }
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

/// Read description from a directory by checking common markdown files in
/// priority order: `IDEA.md` → `README.md` → `CLAUDE.md` → `AGENTS.md`.
pub fn description_from_repo(path: &Path) -> Option<String> {
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

/// Same as `extract_md_description` but with a fallback string.
pub fn read_md_description(path: &Path) -> String {
    extract_md_description(path).unwrap_or_else(|| "No description".to_string())
}

/// Percent-encode a string for use in `obsidian://` URIs. Keeps the
/// allowed-set small to match Obsidian's parser quirks.
pub fn percent_encode(s: &str) -> String {
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

// ── Export rendering ───────────────────────────────────────────────────

/// Sanitize a project name into a safe filename. Replaces filesystem-hostile
/// characters with `-`, collapses runs of `-`, strips control chars, and
/// returns `"untitled"` for an otherwise-empty result.
pub fn sanitize_filename(name: &str) -> String {
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

/// Render a Project as a markdown note with YAML frontmatter.
pub fn render_project_markdown(p: &Project) -> String {
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

    // Links — only durable URLs. The local path lives in frontmatter; we
    // skip machine-specific `vscode://` links so the note is portable.
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

/// Quote a YAML scalar value if it contains characters that would confuse
/// the parser (`: # " ' \n [ ] { }`), starts/ends with whitespace, or
/// starts with `-`.
pub fn yaml_escape(s: &str) -> String {
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

/// Quote a string for use inside a YAML inline list `[a, b, "c d"]`.
pub fn yaml_inline_string(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('[') || s.contains(']') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Walk `projects` and write one `.md` per project to `out_dir`.
/// Returns `(written, errors)`.
pub fn run_export(projects: &[Project], out_dir: &Path) -> Result<(usize, usize), String> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("Cannot create {}: {}", out_dir.display(), e))?;
    let mut written = 0usize;
    let mut errors = 0usize;
    let mut seen_names: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in projects {
        let base = sanitize_filename(&p.name);
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
