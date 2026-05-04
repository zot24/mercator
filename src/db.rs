//! SQLite persistence — schema setup, JSON-to-DB migration, and the
//! read/write helpers the rest of the binary will route through as #24
//! lands in stages.
//!
//! Stage 1 (this commit) only exposes:
//! - `open()` — opens a `Connection` and applies schema v1
//! - `import_from_json()` — one-time idempotent migration from the
//!   existing `mercator_map.json` + `mercator_purged.json` sidecars
//! - `load_all_projects()` / `count_projects` / `count_purged` — read
//!   helpers used by the tests today and by stage-2 handlers later
//!
//! Nothing in `main.rs` reads from the DB yet — the JSON file is still
//! the source of truth. This stage just gets the DB into existence so
//! the user can verify the migration before we cut over.

use crate::project::{load_map, Project, ProjectType};
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    project_type TEXT NOT NULL,
    last_modified TEXT,
    git_branch TEXT,
    last_commit TEXT,
    git_status TEXT,
    remote_url TEXT,
    agent_used TEXT,
    last_seen TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS project_tags (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, tag_id)
);

CREATE TABLE IF NOT EXISTS tech_stack (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS project_tech (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    tech_id INTEGER NOT NULL REFERENCES tech_stack(id) ON DELETE CASCADE,
    PRIMARY KEY (project_id, tech_id)
);

CREATE TABLE IF NOT EXISTS obsidian_links (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    note_path TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS purged (
    path TEXT PRIMARY KEY
);

PRAGMA user_version = 1;
"#;

/// Open or create a SQLite database at `path` and apply schema v1.
///
/// PRAGMAs:
/// - `journal_mode = WAL` — readers don't block writers (and vice versa),
///   which matters once stage 2+ has the dashboard reading concurrently
///   with survey runs.
/// - `foreign_keys = ON` — `ON DELETE CASCADE` on the M2M tables actually
///   fires; SQLite's default is OFF for backwards compat reasons.
pub fn open(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open db {}: {}", path.display(), e))?;
    // `journal_mode = WAL` is a query-style PRAGMA — `execute_batch` accepts
    // it but the result is discarded; that's fine, we just need the side
    // effect.
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .map_err(|e| format!("set pragmas: {}", e))?;
    conn.execute_batch(SCHEMA_V1)
        .map_err(|e| format!("apply schema: {}", e))?;
    Ok(conn)
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub projects_inserted: usize,
    pub projects_updated: usize,
    pub purged_inserted: usize,
}

/// Import an existing `mercator_map.json` (and optional
/// `mercator_purged.json` sidecar) into the DB. Idempotent: re-running
/// upserts each row by `path`. Useful both as the one-time migration
/// the issue calls for, and as a "I edited the JSON, push it through"
/// recovery path during the staged rollout.
pub fn import_from_json(
    conn: &mut Connection,
    map_json_path: &Path,
    purged_json_path: Option<&Path>,
) -> Result<ImportStats, String> {
    let mut stats = ImportStats::default();

    // Projects
    if map_json_path.exists() {
        let projects = load_map(map_json_path).map_err(|e| format!("read map.json: {}", e))?;
        let tx = conn.transaction().map_err(|e| format!("begin tx: {}", e))?;
        for p in &projects {
            let inserted = upsert_project(&tx, p)?;
            if inserted {
                stats.projects_inserted += 1;
            } else {
                stats.projects_updated += 1;
            }
        }
        tx.commit().map_err(|e| format!("commit tx: {}", e))?;
    }

    // Purged sidecar
    if let Some(purged_path) = purged_json_path {
        if purged_path.exists() {
            let raw = std::fs::read_to_string(purged_path)
                .map_err(|e| format!("read purged.json: {}", e))?;
            let paths: HashSet<String> =
                serde_json::from_str(&raw).map_err(|e| format!("parse purged.json: {}", e))?;
            let tx = conn.transaction().map_err(|e| format!("begin tx: {}", e))?;
            for path in &paths {
                let n = tx
                    .execute(
                        "INSERT OR IGNORE INTO purged(path) VALUES (?)",
                        params![path],
                    )
                    .map_err(|e| format!("insert purged: {}", e))?;
                stats.purged_inserted += n;
            }
            tx.commit()
                .map_err(|e| format!("commit purged tx: {}", e))?;
        }
    }

    Ok(stats)
}

/// Insert-or-update a single project keyed on `path`. Returns `true`
/// when a new row was inserted, `false` when an existing row was updated.
/// Within the same transaction also upserts tags, tech_stack, and the
/// optional obsidian_links record.
fn upsert_project(tx: &rusqlite::Transaction, p: &Project) -> Result<bool, String> {
    let project_type = project_type_to_str(&p.project_type);

    // Check existence first — `last_insert_rowid()` doesn't disambiguate
    // INSERT vs UPDATE through `ON CONFLICT DO UPDATE`. Doing the lookup
    // up-front is cheap (path is a UNIQUE INDEX) and gives us a clean
    // signal for the ImportStats counter.
    let pre_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM projects WHERE path = ?",
            params![p.path],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| format!("pre-check project {}: {}", p.path, e))?;
    let inserted = pre_id.is_none();

    // ON CONFLICT(path) DO UPDATE — keeps the existing id, refreshes all
    // metadata fields including last_seen so dashboards can tell whether
    // a project was visited in the most recent survey.
    // `last_seen` is generated by SQLite's `strftime` so the format
    // matches the schema's `DEFAULT` clause exactly — Rust's `chrono`
    // `%f` is 9-digit nanoseconds with no separator, while SQLite's `%f`
    // is `SS.SSS`. Letting SQLite mint the timestamp avoids that mismatch.
    tx.execute(
        "INSERT INTO projects
            (path, name, description, project_type, last_modified, git_branch,
             last_commit, git_status, remote_url, agent_used, last_seen)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(path) DO UPDATE SET
            name = excluded.name,
            description = excluded.description,
            project_type = excluded.project_type,
            last_modified = excluded.last_modified,
            git_branch = excluded.git_branch,
            last_commit = excluded.last_commit,
            git_status = excluded.git_status,
            remote_url = excluded.remote_url,
            agent_used = excluded.agent_used,
            last_seen = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        params![
            p.path,
            p.name,
            p.description,
            project_type,
            p.last_modified,
            p.git_branch,
            p.last_commit,
            p.git_status,
            p.remote_url,
            p.agent_used,
        ],
    )
    .map_err(|e| format!("upsert project {}: {}", p.path, e))?;

    let project_id: i64 = pre_id.unwrap_or_else(|| tx.last_insert_rowid());

    // Replace tag links for this project (drop + re-add — simpler than
    // diffing, and the tag set is tiny).
    tx.execute(
        "DELETE FROM project_tags WHERE project_id = ?",
        params![project_id],
    )
    .map_err(|e| format!("clear project_tags: {}", e))?;
    for tag in &p.tags {
        let tag_id = upsert_lookup(tx, "tags", tag)?;
        tx.execute(
            "INSERT OR IGNORE INTO project_tags(project_id, tag_id) VALUES (?, ?)",
            params![project_id, tag_id],
        )
        .map_err(|e| format!("link tag {}: {}", tag, e))?;
    }

    // Same drop+re-add pattern for tech_stack.
    tx.execute(
        "DELETE FROM project_tech WHERE project_id = ?",
        params![project_id],
    )
    .map_err(|e| format!("clear project_tech: {}", e))?;
    for tech in &p.tech_stack {
        let tech_id = upsert_lookup(tx, "tech_stack", tech)?;
        tx.execute(
            "INSERT OR IGNORE INTO project_tech(project_id, tech_id) VALUES (?, ?)",
            params![project_id, tech_id],
        )
        .map_err(|e| format!("link tech {}: {}", tech, e))?;
    }

    // Obsidian link is optional and 1:1.
    match (&p.obsidian_url, &p.obsidian_note_path) {
        (Some(url), Some(note_path)) => {
            tx.execute(
                "INSERT INTO obsidian_links(project_id, url, note_path)
                 VALUES (?, ?, ?)
                 ON CONFLICT(project_id) DO UPDATE SET
                    url = excluded.url,
                    note_path = excluded.note_path",
                params![project_id, url, note_path],
            )
            .map_err(|e| format!("upsert obsidian_link: {}", e))?;
        }
        _ => {
            tx.execute(
                "DELETE FROM obsidian_links WHERE project_id = ?",
                params![project_id],
            )
            .map_err(|e| format!("clear obsidian_link: {}", e))?;
        }
    }

    Ok(inserted)
}

/// `INSERT OR IGNORE` into a small lookup table, then return the row id.
fn upsert_lookup(tx: &rusqlite::Transaction, table: &str, name: &str) -> Result<i64, String> {
    let insert_sql = format!("INSERT OR IGNORE INTO {table}(name) VALUES (?)");
    tx.execute(&insert_sql, params![name])
        .map_err(|e| format!("insert {table}({}): {}", name, e))?;
    let select_sql = format!("SELECT id FROM {table} WHERE name = ?");
    tx.query_row(&select_sql, params![name], |r| r.get(0))
        .map_err(|e| format!("lookup {table}({}): {}", name, e))
}

fn project_type_to_str(t: &ProjectType) -> &'static str {
    match t {
        ProjectType::Git => "Git",
        ProjectType::Folder => "Folder",
        ProjectType::Idea => "Idea",
        ProjectType::GitHub => "GitHub",
        ProjectType::GitLab => "GitLab",
        ProjectType::Obsidian => "Obsidian",
    }
}

fn project_type_from_str(s: &str) -> Result<ProjectType, String> {
    match s {
        "Git" => Ok(ProjectType::Git),
        "Folder" => Ok(ProjectType::Folder),
        "Idea" => Ok(ProjectType::Idea),
        "GitHub" => Ok(ProjectType::GitHub),
        "GitLab" => Ok(ProjectType::GitLab),
        "Obsidian" => Ok(ProjectType::Obsidian),
        other => Err(format!("unknown project_type {other:?}")),
    }
}

pub fn count_projects(conn: &Connection) -> Result<u64, String> {
    conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get::<_, i64>(0))
        .map(|n| n as u64)
        .map_err(|e| format!("count projects: {}", e))
}

pub fn count_purged(conn: &Connection) -> Result<u64, String> {
    conn.query_row("SELECT COUNT(*) FROM purged", [], |r| r.get::<_, i64>(0))
        .map(|n| n as u64)
        .map_err(|e| format!("count purged: {}", e))
}

/// Read every project out of the DB, fully hydrated (tags, tech_stack,
/// obsidian link). Used by tests today; the stage-2 cutover for #24
/// switches `/api/map` and friends to call this in place of `load_map`.
#[allow(dead_code)]
pub fn load_all_projects(conn: &Connection) -> Result<Vec<Project>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, path, name, description, project_type, last_modified,
                    git_branch, last_commit, git_status, remote_url, agent_used
             FROM projects ORDER BY path",
        )
        .map_err(|e| format!("prepare select: {}", e))?;

    let rows: Vec<(i64, Project)> = stmt
        .query_map([], |r| {
            let project_type_str: String = r.get(4)?;
            let project_type = project_type_from_str(&project_type_str).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    "unknown project_type".into(),
                )
            })?;
            Ok((
                r.get::<_, i64>(0)?,
                Project {
                    name: r.get(2)?,
                    path: r.get(1)?,
                    description: r.get(3)?,
                    project_type,
                    last_modified: r.get(5)?,
                    git_branch: r.get(6)?,
                    last_commit: r.get(7)?,
                    git_status: r.get(8)?,
                    tech_stack: vec![],
                    remote_url: r.get(9)?,
                    agent_used: r.get(10)?,
                    obsidian_url: None,
                    obsidian_note_path: None,
                    tags: vec![],
                },
            ))
        })
        .map_err(|e| format!("query projects: {}", e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read projects: {}", e))?;

    let mut tag_stmt = conn
        .prepare("SELECT t.name FROM tags t JOIN project_tags pt ON pt.tag_id = t.id WHERE pt.project_id = ?")
        .map_err(|e| format!("prepare tags: {}", e))?;
    let mut tech_stmt = conn
        .prepare("SELECT s.name FROM tech_stack s JOIN project_tech ps ON ps.tech_id = s.id WHERE ps.project_id = ?")
        .map_err(|e| format!("prepare tech: {}", e))?;
    let mut obs_stmt = conn
        .prepare("SELECT url, note_path FROM obsidian_links WHERE project_id = ?")
        .map_err(|e| format!("prepare obsidian: {}", e))?;

    let mut projects: Vec<Project> = Vec::with_capacity(rows.len());
    for (id, mut p) in rows {
        let tags: Vec<String> = tag_stmt
            .query_map(params![id], |r| r.get::<_, String>(0))
            .map_err(|e| format!("query tags for {}: {}", id, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("read tags: {}", e))?;
        p.tags = tags;

        let tech: Vec<String> = tech_stmt
            .query_map(params![id], |r| r.get::<_, String>(0))
            .map_err(|e| format!("query tech for {}: {}", id, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("read tech: {}", e))?;
        p.tech_stack = tech;

        if let Ok((url, note_path)) = obs_stmt.query_row(params![id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            p.obsidian_url = Some(url);
            p.obsidian_note_path = Some(note_path);
        }

        projects.push(p);
    }
    Ok(projects)
}

/// Path next to a given DB file or map file — by convention the
/// purged-paths blocklist sits next to the active map.
pub fn purged_sidecar_for_map(map_file: &Path) -> PathBuf {
    let parent = map_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("mercator_purged.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{save_map, Project, ProjectType};

    fn sample_project(name: &str, path: &str, ptype: ProjectType) -> Project {
        Project {
            name: name.to_string(),
            path: path.to_string(),
            description: format!("desc for {name}"),
            project_type: ptype,
            last_modified: Some("2026-05-04T00:00:00Z".to_string()),
            git_branch: Some("master".to_string()),
            last_commit: Some("init".to_string()),
            git_status: None,
            tech_stack: vec!["Rust".to_string(), "CLI".to_string()],
            remote_url: Some(format!("https://example.test/{name}")),
            agent_used: Some("claude".to_string()),
            obsidian_url: None,
            obsidian_note_path: None,
            tags: vec!["tag-a".to_string(), "tag-b".to_string()],
        }
    }

    #[test]
    fn open_creates_schema_v1() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("test.db")).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        // All seven tables exist.
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for expected in [
            "obsidian_links",
            "project_tags",
            "project_tech",
            "projects",
            "purged",
            "tags",
            "tech_stack",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing table {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn import_round_trips_project_shape() {
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("mercator_map.json");
        let projects_in = vec![
            sample_project("alpha", "/tmp/alpha", ProjectType::Git),
            sample_project("beta", "/tmp/beta", ProjectType::GitHub),
        ];
        save_map(&projects_in, &map_path).unwrap();

        let mut conn = open(&dir.path().join("mercator.db")).unwrap();
        let stats = import_from_json(&mut conn, &map_path, None).unwrap();
        assert_eq!(stats.projects_inserted, 2);
        assert_eq!(stats.projects_updated, 0);
        assert_eq!(count_projects(&conn).unwrap(), 2);

        let projects_out = load_all_projects(&conn).unwrap();
        assert_eq!(projects_out.len(), 2);
        let alpha = projects_out.iter().find(|p| p.name == "alpha").unwrap();
        assert_eq!(alpha.path, "/tmp/alpha");
        assert!(matches!(alpha.project_type, ProjectType::Git));
        assert_eq!(alpha.tags, vec!["tag-a".to_string(), "tag-b".to_string()]);
        assert!(alpha.tech_stack.contains(&"Rust".to_string()));
        assert!(alpha.tech_stack.contains(&"CLI".to_string()));
        assert_eq!(alpha.agent_used.as_deref(), Some("claude"));
    }

    #[test]
    fn repeated_import_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        save_map(
            &[sample_project("alpha", "/tmp/alpha", ProjectType::Git)],
            &map_path,
        )
        .unwrap();

        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        let s1 = import_from_json(&mut conn, &map_path, None).unwrap();
        assert_eq!(s1.projects_inserted, 1);
        let s2 = import_from_json(&mut conn, &map_path, None).unwrap();
        assert_eq!(s2.projects_inserted, 0);
        assert_eq!(s2.projects_updated, 1);
        // Tag dedup: still only two tag rows (tag-a, tag-b), not four.
        let tag_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag_count, 2);
    }

    #[test]
    fn import_updates_changed_fields() {
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        let mut p = sample_project("alpha", "/tmp/alpha", ProjectType::Git);
        save_map(&[p.clone()], &map_path).unwrap();

        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        import_from_json(&mut conn, &map_path, None).unwrap();

        // Mutate description + tags + add an obsidian link, re-import,
        // verify the row carries the new shape.
        p.description = "updated desc".to_string();
        p.tags = vec!["tag-c".to_string()];
        p.obsidian_url = Some("obsidian://x".to_string());
        p.obsidian_note_path = Some("Projects/alpha".to_string());
        save_map(&[p], &map_path).unwrap();
        import_from_json(&mut conn, &map_path, None).unwrap();

        let out = load_all_projects(&conn).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].description, "updated desc");
        assert_eq!(out[0].tags, vec!["tag-c".to_string()]);
        assert_eq!(out[0].obsidian_url.as_deref(), Some("obsidian://x"));
    }

    #[test]
    fn import_purged_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        let empty: Vec<Project> = Vec::new();
        save_map(&empty, &map_path).unwrap();
        let purged_path = dir.path().join("mercator_purged.json");
        std::fs::write(
            &purged_path,
            r#"["/tmp/alpha","/tmp/beta","/tmp/alpha"]"#, // duplicate handled by HashSet
        )
        .unwrap();

        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        let stats = import_from_json(&mut conn, &map_path, Some(&purged_path)).unwrap();
        assert_eq!(stats.purged_inserted, 2);
        assert_eq!(count_purged(&conn).unwrap(), 2);

        // Re-import is a no-op for the purged table (INSERT OR IGNORE).
        let stats2 = import_from_json(&mut conn, &map_path, Some(&purged_path)).unwrap();
        assert_eq!(stats2.purged_inserted, 0);
        assert_eq!(count_purged(&conn).unwrap(), 2);
    }

    #[test]
    fn last_seen_uses_iso8601_with_milliseconds() {
        // Regression guard: the SS.SSS format mismatch between Rust's chrono
        // `%f` (9-digit nanos, no separator) and SQLite's `%f` (`SS.SSS`)
        // produced timestamps like `T16:31:586797000Z`. SQLite mints the
        // value via `strftime` so both INSERT and UPDATE branches agree.
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        save_map(
            &[sample_project("alpha", "/tmp/alpha", ProjectType::Git)],
            &map_path,
        )
        .unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        import_from_json(&mut conn, &map_path, None).unwrap();

        let last_seen: String = conn
            .query_row(
                "SELECT last_seen FROM projects WHERE path = ?",
                params!["/tmp/alpha"],
                |r| r.get(0),
            )
            .unwrap();
        // Shape: 2026-05-04T13:31:40.300Z — single decimal point in the
        // seconds field, then a Z suffix.
        assert!(last_seen.ends_with('Z'), "got {last_seen:?}");
        let secs_section = last_seen
            .split('T')
            .nth(1)
            .and_then(|t| t.split(':').nth(2))
            .unwrap();
        assert!(
            secs_section.contains('.'),
            "expected decimal in seconds, got {secs_section:?} (full: {last_seen:?})"
        );
    }

    #[test]
    fn purged_sidecar_for_map_sits_next_to_map() {
        assert_eq!(
            purged_sidecar_for_map(Path::new("/tmp/x/mercator_map.json")),
            PathBuf::from("/tmp/x/mercator_purged.json"),
        );
        // Bare filename: Path::parent() returns Some("") (not None), so the
        // join produces a relative path without an explicit "./" prefix.
        // Same shape as the existing `purge_file_path` helper.
        assert_eq!(
            purged_sidecar_for_map(Path::new("map.json")),
            PathBuf::from("mercator_purged.json"),
        );
    }
}
