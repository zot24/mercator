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
"#;

/// Schema v2: add the FTS5 virtual table over name + description + tags.
/// Default-mode FTS5 (no `content=` clause) — stores its own copy of the
/// indexed columns. Slightly redundant with the `projects` table but
/// supports plain `DELETE FROM` (contentless and external-content modes
/// don't, which makes per-row sync from `upsert_project` awkward).
const SCHEMA_V2: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS projects_fts USING fts5(
    name,
    description,
    tags
);
"#;

/// Open or create a SQLite database at `path`, apply the schema, and
/// migrate to the latest version.
///
/// PRAGMAs:
/// - `journal_mode = WAL` — readers don't block writers (and vice versa),
///   which matters once stage 2+ has the dashboard reading concurrently
///   with survey runs.
/// - `foreign_keys = ON` — `ON DELETE CASCADE` on the M2M tables actually
///   fires; SQLite's default is OFF for backwards compat reasons.
pub fn open(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open db {}: {}", path.display(), e))?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .map_err(|e| format!("set pragmas: {}", e))?;
    conn.execute_batch(SCHEMA_V1)
        .map_err(|e| format!("apply schema v1: {}", e))?;

    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("read user_version: {}", e))?;

    if user_version < 2 {
        conn.execute_batch(SCHEMA_V2)
            .map_err(|e| format!("apply schema v2: {}", e))?;
        rebuild_fts(&conn)?;
        conn.execute_batch("PRAGMA user_version = 2;")
            .map_err(|e| format!("bump user_version to 2: {}", e))?;
    }
    Ok(conn)
}

/// Repopulate `projects_fts` from the current `projects` + `project_tags`
/// state. Used by the v1→v2 migration and exposed for tests; production
/// code should rely on `upsert_project` keeping the index in sync incrementally.
fn rebuild_fts(conn: &Connection) -> Result<(), String> {
    conn.execute("DELETE FROM projects_fts", [])
        .map_err(|e| format!("clear projects_fts: {}", e))?;
    let mut select = conn
        .prepare(
            "SELECT p.id, p.name, p.description,
                    COALESCE((SELECT GROUP_CONCAT(t.name, ' ')
                              FROM project_tags pt
                              JOIN tags t ON pt.tag_id = t.id
                              WHERE pt.project_id = p.id), '')
             FROM projects p",
        )
        .map_err(|e| format!("prepare rebuild: {}", e))?;
    let rows: Vec<(i64, String, String, String)> = select
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| format!("query rebuild: {}", e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read rebuild: {}", e))?;
    let mut insert = conn
        .prepare("INSERT INTO projects_fts(rowid, name, description, tags) VALUES (?, ?, ?, ?)")
        .map_err(|e| format!("prepare fts insert: {}", e))?;
    for (id, name, description, tags) in rows {
        insert
            .execute(params![id, name, description, tags])
            .map_err(|e| format!("insert fts row {}: {}", id, e))?;
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub projects_inserted: usize,
    pub projects_updated: usize,
    pub purged_inserted: usize,
}

/// Import an existing `mercator_map.json` (and optional
/// `mercator_purged.json` sidecar) into the DB. Idempotent: re-running
/// upserts each row by `path`. Used both as the one-time migration the
/// issue calls for, and as the per-survey hydration path during the
/// staged rollout.
///
/// The purged sidecar is imported **first** and projects whose path is
/// on the blocklist (either the live DB row or the freshly-imported
/// sidecar entries) are skipped — otherwise the stale `map.json`
/// snapshot from before a dashboard purge would silently re-introduce
/// purged projects on the next survey.
pub fn import_from_json(
    conn: &mut Connection,
    map_json_path: &Path,
    purged_json_path: Option<&Path>,
) -> Result<ImportStats, String> {
    let mut stats = ImportStats::default();

    // Purged sidecar first so the project loop below can skip purged paths.
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

    // Projects, with blocklist filter.
    if map_json_path.exists() {
        let projects = load_map(map_json_path).map_err(|e| format!("read map.json: {}", e))?;
        let purged_paths: HashSet<String> = list_purged(conn)?.into_iter().collect();
        let kept: Vec<Project> = projects
            .into_iter()
            .filter(|p| !purged_paths.contains(p.path.trim_end_matches('/')))
            .collect();
        let upsert_stats = upsert_projects(conn, &kept)?;
        stats.projects_inserted = upsert_stats.projects_inserted;
        stats.projects_updated = upsert_stats.projects_updated;
    }

    Ok(stats)
}

/// Bulk upsert a slice of projects in a single transaction. Returns
/// counts of newly-inserted vs updated rows. Used by stage-2c handlers
/// that already hold a `Vec<Project>` (refresh re-survey, recategorize)
/// and want to push it to the DB without going through a JSON file.
pub fn upsert_projects(conn: &mut Connection, projects: &[Project]) -> Result<ImportStats, String> {
    let mut stats = ImportStats::default();
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin upsert tx: {}", e))?;
    for p in projects {
        let inserted = upsert_project(&tx, p)?;
        if inserted {
            stats.projects_inserted += 1;
        } else {
            stats.projects_updated += 1;
        }
    }
    tx.commit()
        .map_err(|e| format!("commit upsert tx: {}", e))?;
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

    // Sync the FTS5 row. Contentless table → DELETE+INSERT keyed on the
    // projects.id rowid. Tags get joined into a space-separated string
    // because FTS5 doesn't index list values directly. Skip silently if
    // the FTS table doesn't exist yet (legacy v1 DBs that haven't been
    // through `open()`'s migration).
    let fts_exists: bool = tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='projects_fts'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if fts_exists {
        let tags_joined = p.tags.join(" ");
        tx.execute(
            "DELETE FROM projects_fts WHERE rowid = ?",
            params![project_id],
        )
        .map_err(|e| format!("clear fts row {}: {}", project_id, e))?;
        tx.execute(
            "INSERT INTO projects_fts(rowid, name, description, tags) VALUES (?, ?, ?, ?)",
            params![project_id, p.name, p.description, tags_joined],
        )
        .map_err(|e| format!("insert fts row {}: {}", project_id, e))?;
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

/// Atomically purge a project: add it to the blocklist, drop the
/// `projects` row (FK cascades clean up M2M + obsidian links), and
/// remove the corresponding `projects_fts` row so it doesn't show up
/// in search results.
/// Returns `(blocklist_was_new, project_was_present)`.
pub fn purge_project(conn: &mut Connection, path: &str) -> Result<(bool, bool), String> {
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin purge tx: {}", e))?;
    let blocklist_inserted = tx
        .execute(
            "INSERT OR IGNORE INTO purged(path) VALUES (?)",
            params![path],
        )
        .map_err(|e| format!("insert purged({}): {}", path, e))?
        == 1;
    // Capture the project's id before delete so we can clean its FTS row.
    let project_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM projects WHERE path = ?",
            params![path],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| format!("lookup id for purge({}): {}", path, e))?;
    let project_deleted = tx
        .execute("DELETE FROM projects WHERE path = ?", params![path])
        .map_err(|e| format!("delete project({}): {}", path, e))?
        == 1;
    if let Some(id) = project_id {
        // FTS5 contentless table: explicit DELETE keyed on the rowid we
        // know used to align with projects.id.
        let _ = tx.execute("DELETE FROM projects_fts WHERE rowid = ?", params![id]);
    }
    tx.commit().map_err(|e| format!("commit purge: {}", e))?;
    Ok((blocklist_inserted, project_deleted))
}

/// Remove a path from the blocklist. Returns `true` if a row was actually
/// removed (i.e. it was previously purged).
pub fn restore_purged(conn: &Connection, path: &str) -> Result<bool, String> {
    conn.execute("DELETE FROM purged WHERE path = ?", params![path])
        .map(|n| n == 1)
        .map_err(|e| format!("delete purged({}): {}", path, e))
}

/// List purged paths sorted alphabetically — matches the order the JSON
/// sidecar handler returned today, so the dashboard's settings panel
/// doesn't see a behavior change.
pub fn list_purged(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT path FROM purged ORDER BY path")
        .map_err(|e| format!("prepare list_purged: {}", e))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| format!("query purged: {}", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read purged: {}", e))
}

const PROJECT_COLUMNS: &str =
    "p.id, p.path, p.name, p.description, p.project_type, p.last_modified,
            p.git_branch, p.last_commit, p.git_status, p.remote_url, p.agent_used";

fn project_from_row(r: &rusqlite::Row) -> rusqlite::Result<(i64, Project)> {
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
}

/// Hydrate the M2M relations (tags, tech_stack, obsidian link) for a
/// freshly-loaded list of `(id, Project)` rows. Lifted out so
/// `load_all_projects`, `search_projects`, and `list_projects` share the
/// same join logic.
fn hydrate_relations(conn: &Connection, rows: Vec<(i64, Project)>) -> Result<Vec<Project>, String> {
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

/// Read every project out of the DB, fully hydrated (tags, tech_stack,
/// obsidian link).
pub fn load_all_projects(conn: &Connection) -> Result<Vec<Project>, String> {
    let sql = format!("SELECT {} FROM projects p ORDER BY p.path", PROJECT_COLUMNS);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare select: {}", e))?;
    let rows: Vec<(i64, Project)> = stmt
        .query_map([], project_from_row)
        .map_err(|e| format!("query projects: {}", e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read projects: {}", e))?;
    hydrate_relations(conn, rows)
}

/// Full-text search across name + description + tags via FTS5. Results
/// come back ordered by FTS5 rank (best match first), fully hydrated.
///
/// User input is tokenized on whitespace and each token is double-quoted
/// before being passed to `MATCH`, so punctuation inside a word
/// (hyphens, dots, slashes) doesn't confuse the FTS5 parser — bare
/// `cli-tool` would otherwise parse as `cli NOT tool`. Multi-word
/// queries become AND across tokens. Users who want raw FTS5 syntax
/// (prefix `foo*`, `OR`, column filters) can wrap the whole query in
/// quotes from the shell, e.g. `mercator search '"foo OR bar"'`.
pub fn search_projects(conn: &Connection, query: &str) -> Result<Vec<Project>, String> {
    let normalized = normalize_fts_query(query);
    let sql = format!(
        "SELECT {} FROM projects p
         JOIN projects_fts f ON p.id = f.rowid
         WHERE projects_fts MATCH ?
         ORDER BY f.rank",
        PROJECT_COLUMNS
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare search: {}", e))?;
    let rows: Vec<(i64, Project)> = stmt
        .query_map(params![normalized], project_from_row)
        .map_err(|e| format!("query search ({:?}): {}", normalized, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read search rows: {}", e))?;
    hydrate_relations(conn, rows)
}

/// Wrap each whitespace-separated token in double-quotes so FTS5 treats
/// it as a literal phrase. Internal double-quotes are escaped per the
/// FTS5 spec (`""`). Empty input yields empty output (the caller's
/// MATCH will return zero rows).
fn normalize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Filters for `list_projects`. Each `Some` narrows the result set;
/// `None` leaves that dimension unfiltered. Combining filters is AND.
#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    pub project_type: Option<String>,
    pub tag: Option<String>,
    pub tech: Option<String>,
}

/// Filtered project list (no FTS, just SQL filters). Used by
/// `mercator list` which has predicate filters but no free-text needs.
pub fn list_projects(conn: &Connection, filter: &ListFilter) -> Result<Vec<Project>, String> {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(t) = &filter.project_type {
        clauses.push("p.project_type = ?".into());
        binds.push(t.clone());
    }
    if let Some(tag) = &filter.tag {
        clauses.push(
            "p.id IN (SELECT pt.project_id FROM project_tags pt
                      JOIN tags t ON pt.tag_id = t.id WHERE t.name = ?)"
                .into(),
        );
        binds.push(tag.clone());
    }
    if let Some(tech) = &filter.tech {
        clauses.push(
            "p.id IN (SELECT ps.project_id FROM project_tech ps
                      JOIN tech_stack s ON ps.tech_id = s.id WHERE s.name = ?)"
                .into(),
        );
        binds.push(tech.clone());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT {} FROM projects p{} ORDER BY p.path",
        PROJECT_COLUMNS, where_sql,
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare list: {}", e))?;
    let bound: Vec<&dyn rusqlite::ToSql> =
        binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows: Vec<(i64, Project)> = stmt
        .query_map(bound.as_slice(), project_from_row)
        .map_err(|e| format!("query list: {}", e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read list rows: {}", e))?;
    hydrate_relations(conn, rows)
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
    fn open_creates_full_schema() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("test.db")).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
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
            "projects_fts",
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
    fn import_skips_paths_on_the_purge_blocklist() {
        // Regression guard for a stage-3b bug: a stale `map.json` (written
        // before a dashboard-side purge) used to silently re-introduce
        // the purged project on the next survey because `import_from_json`
        // upserted projects without consulting the blocklist. The fix
        // imports the sidecar first and skips already-purged paths.
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        let purged_path = dir.path().join("mercator_purged.json");
        save_map(
            &[
                sample_project("alpha", "/tmp/alpha", ProjectType::Git),
                sample_project("beta", "/tmp/beta", ProjectType::Git),
            ],
            &map_path,
        )
        .unwrap();
        std::fs::write(&purged_path, r#"["/tmp/alpha"]"#).unwrap();

        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        let stats = import_from_json(&mut conn, &map_path, Some(&purged_path)).unwrap();
        assert_eq!(stats.projects_inserted, 1, "alpha should be skipped");
        assert_eq!(count_projects(&conn).unwrap(), 1);
        assert_eq!(count_purged(&conn).unwrap(), 1);

        let projects = load_all_projects(&conn).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "beta");
    }

    #[test]
    fn purge_project_drops_row_and_cascades() {
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        let mut p = sample_project("alpha", "/tmp/alpha", ProjectType::Git);
        p.obsidian_url = Some("obsidian://x".to_string());
        p.obsidian_note_path = Some("Projects/alpha".to_string());
        save_map(&[p], &map_path).unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        import_from_json(&mut conn, &map_path, None).unwrap();

        let (blocklist_new, project_deleted) = purge_project(&mut conn, "/tmp/alpha").unwrap();
        assert!(blocklist_new);
        assert!(project_deleted);
        assert_eq!(count_projects(&conn).unwrap(), 0);
        assert_eq!(count_purged(&conn).unwrap(), 1);

        // FK cascades dropped the M2M and obsidian rows.
        let tag_links: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_tags", [], |r| r.get(0))
            .unwrap();
        let tech_links: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_tech", [], |r| r.get(0))
            .unwrap();
        let obs_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM obsidian_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag_links, 0);
        assert_eq!(tech_links, 0);
        assert_eq!(obs_rows, 0);
    }

    #[test]
    fn purge_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let map_path = dir.path().join("map.json");
        save_map(
            &[sample_project("alpha", "/tmp/alpha", ProjectType::Git)],
            &map_path,
        )
        .unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        import_from_json(&mut conn, &map_path, None).unwrap();

        let first = purge_project(&mut conn, "/tmp/alpha").unwrap();
        assert_eq!(first, (true, true));
        let second = purge_project(&mut conn, "/tmp/alpha").unwrap();
        // Already on the blocklist, project already gone.
        assert_eq!(second, (false, false));
    }

    #[test]
    fn restore_purged_removes_from_blocklist() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("db.sqlite")).unwrap();
        conn.execute("INSERT INTO purged(path) VALUES ('/tmp/x'), ('/tmp/y')", [])
            .unwrap();
        assert_eq!(count_purged(&conn).unwrap(), 2);

        assert!(restore_purged(&conn, "/tmp/x").unwrap());
        // Restoring an already-restored path is a no-op (returns false).
        assert!(!restore_purged(&conn, "/tmp/x").unwrap());
        assert_eq!(count_purged(&conn).unwrap(), 1);
    }

    #[test]
    fn list_purged_returns_sorted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("db.sqlite")).unwrap();
        // Insert in non-sorted order.
        for path in ["/tmp/c", "/tmp/a", "/tmp/b"] {
            conn.execute("INSERT INTO purged(path) VALUES (?)", params![path])
                .unwrap();
        }
        assert_eq!(
            list_purged(&conn).unwrap(),
            vec![
                "/tmp/a".to_string(),
                "/tmp/b".to_string(),
                "/tmp/c".to_string()
            ]
        );
    }

    // ── FTS5 + filtered list (#24 stage 4a) ────────────────────────────

    #[test]
    fn schema_v2_creates_projects_fts() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("db.sqlite")).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='projects_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
    }

    #[test]
    fn search_projects_finds_by_name_description_tags() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        let mut alpha = sample_project("alpha", "/tmp/alpha", ProjectType::Git);
        alpha.description = "A docker workflow tool".into();
        alpha.tags = vec!["devops".into(), "automation".into()];
        let mut beta = sample_project("beta", "/tmp/beta", ProjectType::Git);
        beta.description = "A static site generator".into();
        beta.tags = vec!["web".into()];
        upsert_projects(&mut conn, &[alpha, beta]).unwrap();

        // Match by name fragment.
        let by_name = search_projects(&conn, "alpha").unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "alpha");

        // Match by description term.
        let by_desc = search_projects(&conn, "docker").unwrap();
        assert_eq!(by_desc.len(), 1);
        assert_eq!(by_desc[0].name, "alpha");

        // Match by tag.
        let by_tag = search_projects(&conn, "automation").unwrap();
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].name, "alpha");

        // No match.
        let none = search_projects(&conn, "rocket").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn search_handles_hyphenated_names_and_multi_word_and() {
        // Regression guard: bare `cli-tool` used to fail with "no such
        // column: tool" because FTS5's parser reads `-` as NOT. The
        // search helper now wraps each token in phrase quotes.
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        let mut a = sample_project("cli-tool", "/tmp/cli-tool", ProjectType::Git);
        a.description = "rust cli for stuff".into();
        let mut b = sample_project("web-app", "/tmp/web-app", ProjectType::Git);
        b.description = "node web frontend".into();
        upsert_projects(&mut conn, &[a, b]).unwrap();

        // Hyphenated literal — punctuation no longer breaks the parse.
        let cli = search_projects(&conn, "cli-tool").unwrap();
        assert_eq!(cli.len(), 1);
        assert_eq!(cli[0].name, "cli-tool");

        // Multi-word AND — both tokens must appear somewhere across the
        // indexed columns.
        let rust_cli = search_projects(&conn, "rust cli").unwrap();
        assert_eq!(rust_cli.len(), 1);
        assert_eq!(rust_cli[0].name, "cli-tool");

        // No match because "rust" doesn't appear in the web-app row.
        let rust_web = search_projects(&conn, "rust web").unwrap();
        assert!(rust_web.is_empty());
    }

    #[test]
    fn normalize_fts_query_quotes_every_token() {
        assert_eq!(normalize_fts_query("foo"), r#""foo""#);
        assert_eq!(normalize_fts_query("foo bar"), r#""foo" "bar""#);
        assert_eq!(normalize_fts_query("cli-tool"), r#""cli-tool""#);
        // Internal double-quote is escaped per FTS5 ("").
        assert_eq!(normalize_fts_query(r#"a"b"#), r#""a""b""#);
        assert_eq!(normalize_fts_query("   "), "");
    }

    #[test]
    fn search_after_purge_excludes_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        upsert_projects(
            &mut conn,
            &[sample_project("alpha", "/tmp/alpha", ProjectType::Git)],
        )
        .unwrap();
        assert_eq!(search_projects(&conn, "alpha").unwrap().len(), 1);

        purge_project(&mut conn, "/tmp/alpha").unwrap();
        // FTS row was cleaned up alongside the projects row.
        assert!(search_projects(&conn, "alpha").unwrap().is_empty());
    }

    #[test]
    fn list_projects_filters_by_type_tag_and_tech() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open(&dir.path().join("db.sqlite")).unwrap();
        let mut a = sample_project("alpha", "/tmp/alpha", ProjectType::Git);
        a.tech_stack = vec!["Rust".into()];
        a.tags = vec!["cli".into()];
        let mut b = sample_project("beta", "/tmp/beta", ProjectType::GitHub);
        b.tech_stack = vec!["Go".into()];
        b.tags = vec!["cli".into()];
        let mut c = sample_project("gamma", "/tmp/gamma", ProjectType::Folder);
        c.tech_stack = vec!["Rust".into()];
        c.tags = vec!["docs".into()];
        upsert_projects(&mut conn, &[a, b, c]).unwrap();

        // No filter → all three.
        let all = list_projects(&conn, &ListFilter::default()).unwrap();
        assert_eq!(all.len(), 3);

        // Type filter.
        let only_git = list_projects(
            &conn,
            &ListFilter {
                project_type: Some("Git".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(only_git.len(), 1);
        assert_eq!(only_git[0].name, "alpha");

        // Tag filter.
        let cli = list_projects(
            &conn,
            &ListFilter {
                tag: Some("cli".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let names: Vec<&str> = cli.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);

        // Tech filter.
        let rust = list_projects(
            &conn,
            &ListFilter {
                tech: Some("Rust".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let names: Vec<&str> = rust.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "gamma"]);

        // AND across filters: Rust + cli → alpha only.
        let combo = list_projects(
            &conn,
            &ListFilter {
                tech: Some("Rust".into()),
                tag: Some("cli".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(combo.len(), 1);
        assert_eq!(combo[0].name, "alpha");
    }

    #[test]
    fn v1_to_v2_migration_rebuilds_fts_from_existing_data() {
        // Simulate a stage-1 DB by creating the v1 schema and inserting a row,
        // then re-opening it and checking the FTS rebuild populated correctly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE projects (
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
                CREATE TABLE tags (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
                CREATE TABLE project_tags (project_id INTEGER, tag_id INTEGER, PRIMARY KEY(project_id, tag_id));
                CREATE TABLE tech_stack (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE);
                CREATE TABLE project_tech (project_id INTEGER, tech_id INTEGER, PRIMARY KEY(project_id, tech_id));
                CREATE TABLE obsidian_links (project_id INTEGER PRIMARY KEY, url TEXT NOT NULL, note_path TEXT NOT NULL);
                CREATE TABLE purged (path TEXT PRIMARY KEY);
                INSERT INTO projects(path, name, description, project_type)
                    VALUES('/tmp/alpha', 'alpha', 'a v1 row to migrate', 'Git');
                INSERT INTO tags(name) VALUES('legacy');
                INSERT INTO project_tags(project_id, tag_id) VALUES(1, 1);
                PRAGMA user_version = 1;
                "#,
            )
            .unwrap();
        }

        // Re-open via the public API — this should run the v2 migration.
        let conn = open(&path).unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            2
        );
        let hits = search_projects(&conn, "v1").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "alpha");
        // Tags from project_tags are joined in.
        let tag_hits = search_projects(&conn, "legacy").unwrap();
        assert_eq!(tag_hits.len(), 1);
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
