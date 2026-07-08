//! GitHub **issue ingestion** — an [`Enrichment`] provider that surveys the
//! issues of **your** GitHub repos and stores them for the dashboard's
//! read-only kanban + per-project drill-down.
//!
//! "Your" repos = the ones the authenticated token can push to (owned, org,
//! or collaborations). A repo-metadata pre-check ([`fetch_repo_meta`]) filters
//! out read-only forks and repos with Issues disabled *before* fetching
//! issues, so the board stays scoped to what you actually work on and no call
//! is wasted 404ing on a disabled tracker.
//!
//! This is the write side the pre-existing scaffold in this file only sketched
//! (it had read structs but no fetcher and targeted tables that were never
//! created). It now owns the full loop: repo pre-check + issue fetch
//! (`reqwest`, paginated, same auth convention as `sources::fetch_github_repos`
//! and `ticket.rs`), a pure JSON→[`Issue`] parse, persistence into
//! `github_issues`/`github_remotes` (schema v6), and a pure state/label→[`kanban
//! lane`](issue_lane) mapping.
//!
//! Two read surfaces: [`list_issue_views`] powers the all-projects kanban
//! (`/api/issues`), and [`list_issue_views_for_remote`] powers the per-project
//! view (`/api/project/issues`). Both are read-only — cards link to GitHub.
//! Columns derive locally from issue state + a `status:` label convention (no
//! GitHub Projects v2 dependency).

use crate::enrichment::{record_state, Enrichment, EnrichmentError, EnrichmentSummary};
use crate::project::Project;
use crate::sources::{normalize_remote_url, parse_link_next};
use rusqlite::{params, Connection};
use std::path::Path;

/// A GitHub issue as stored in `github_issues`. Field order of [`from_row`]
/// matches the table's column order (schema v6).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Issue {
    #[serde(rename = "remoteUrl")]
    pub remote_url: String,
    #[serde(rename = "number")]
    pub issue_number: i64,
    pub title: String,
    pub state: String,
    #[serde(rename = "author")]
    pub author_login: Option<String>,
    #[serde(rename = "assignees")]
    pub assignees_csv: Option<String>,
    #[serde(rename = "labels")]
    pub labels_csv: Option<String>,
    #[serde(rename = "isPr")]
    pub is_pr: bool,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "closedAt")]
    pub closed_at: Option<String>,
    pub url: String,
}

impl Issue {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            remote_url: r.get(0)?,
            issue_number: r.get(1)?,
            title: r.get(2)?,
            state: r.get(3)?,
            author_login: r.get(4)?,
            assignees_csv: r.get(5)?,
            labels_csv: r.get(6)?,
            is_pr: r.get::<_, i64>(7)? != 0,
            created_at: r.get(8)?,
            updated_at: r.get(9)?,
            closed_at: r.get(10)?,
            url: r.get(11)?,
        })
    }
}

/// An issue plus its computed kanban lane, for the `/api/issues` payload.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IssueView {
    #[serde(flatten)]
    pub issue: Issue,
    pub lane: &'static str,
}

/// The kanban columns, in display order. The dashboard renders one column per
/// entry; every issue maps to exactly one via [`issue_lane`].
pub const LANES: [&str; 3] = ["todo", "doing", "done"];

/// Map an issue's `state` + comma-separated label names to a kanban lane.
/// Pure so it's trivially unit-testable and cheap to re-tune:
/// - closed → `done`
/// - open + a `status:doing` / `in progress` label → `doing`
/// - open + a `status:done` label → `done` (done but not yet closed)
/// - everything else open (incl. untriaged) → `todo`
pub fn issue_lane(state: &str, labels_csv: Option<&str>) -> &'static str {
    if state.eq_ignore_ascii_case("closed") {
        return "done";
    }
    let labels = labels_csv.unwrap_or("").to_lowercase();
    let has = |needle: &str| labels.split(',').any(|l| l.trim() == needle);
    if has("status:doing") || has("in progress") || has("in-progress") || has("doing") {
        return "doing";
    }
    if has("status:done") {
        return "done";
    }
    "todo"
}

/// Parse an `(owner, repo)` pair from a project's git remote, if it's a
/// GitHub remote. Reuses [`normalize_remote_url`] so `https`, `ssh`, and
/// SCP-style remotes all collapse to `github.com/owner/repo` first.
pub fn parse_owner_repo(remote_url: &str) -> Option<(String, String)> {
    let norm = normalize_remote_url(remote_url);
    let rest = norm.strip_prefix("github.com/")?;
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some((owner.to_string(), repo.to_string()))
}

// ── raw API shapes ──────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct GhUser {
    login: String,
}

#[derive(serde::Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(serde::Deserialize)]
struct GhIssueRaw {
    number: i64,
    title: String,
    state: String,
    user: Option<GhUser>,
    #[serde(default)]
    assignees: Vec<GhUser>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    /// Present iff this "issue" is really a pull request — GitHub's issues
    /// endpoint returns both.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: String,
}

/// Parse one page of the GitHub issues API into [`Issue`]s. Pure — no I/O.
pub fn issues_from_json(remote_url: &str, body: &str) -> Result<Vec<Issue>, EnrichmentError> {
    let raw: Vec<GhIssueRaw> =
        serde_json::from_str(body).map_err(|e| EnrichmentError::Parse(e.to_string()))?;
    Ok(raw
        .into_iter()
        .map(|i| {
            let csv = |names: Vec<String>| {
                if names.is_empty() {
                    None
                } else {
                    Some(names.join(","))
                }
            };
            Issue {
                remote_url: remote_url.to_string(),
                issue_number: i.number,
                title: i.title,
                state: i.state,
                author_login: i.user.map(|u| u.login),
                assignees_csv: csv(i.assignees.into_iter().map(|a| a.login).collect()),
                labels_csv: csv(i.labels.into_iter().map(|l| l.name).collect()),
                is_pr: i.pull_request.is_some(),
                created_at: i.created_at,
                updated_at: i.updated_at,
                closed_at: i.closed_at,
                url: i.html_url,
            }
        })
        .collect())
}

/// Minimal repo metadata used to decide whether a repo is "mine" and worth
/// fetching issues for. `permissions` is only populated on authenticated
/// requests — it reflects the token user's access to this repo.
#[derive(serde::Deserialize)]
struct RepoMeta {
    #[serde(default = "default_true")]
    has_issues: bool,
    #[serde(default)]
    permissions: Option<RepoPerms>,
}

#[derive(serde::Deserialize, Default)]
struct RepoPerms {
    #[serde(default)]
    push: bool,
    #[serde(default)]
    admin: bool,
}

fn default_true() -> bool {
    true
}

impl RepoMeta {
    /// True when the authenticated user can push (or admin) — the signal for
    /// "this is one of my projects" that captures owned repos, org repos, and
    /// collaborations while excluding read-only forks.
    fn can_push(&self) -> bool {
        self.permissions
            .as_ref()
            .map(|p| p.push || p.admin)
            .unwrap_or(false)
    }

    /// True when the user administers the repo — the stricter "I own this"
    /// signal that excludes push-only collaborations.
    fn is_admin(&self) -> bool {
        self.permissions.as_ref().map(|p| p.admin).unwrap_or(false)
    }
}

async fn fetch_repo_meta(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<RepoMeta, EnrichmentError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}");
    let mut req = client
        .get(&url)
        .header("User-Agent", "Mercator/1.0")
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| EnrichmentError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(EnrichmentError::Api { status, body });
    }
    let body = resp
        .text()
        .await
        .map_err(|e| EnrichmentError::Network(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| EnrichmentError::Parse(e.to_string()))
}

async fn fetch_repo_issues(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<Vec<Issue>, EnrichmentError> {
    let remote_url = format!("https://github.com/{owner}/{repo}");
    let mut next = Some(format!(
        "https://api.github.com/repos/{owner}/{repo}/issues?state=all&per_page=100"
    ));
    let mut out = Vec::new();
    while let Some(url) = next.take() {
        let mut req = client
            .get(&url)
            .header("User-Agent", "Mercator/1.0")
            .header("Accept", "application/vnd.github+json");
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| EnrichmentError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            return Err(EnrichmentError::Api { status, body });
        }
        let link = resp
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp
            .text()
            .await
            .map_err(|e| EnrichmentError::Network(e.to_string()))?;
        out.append(&mut issues_from_json(&remote_url, &body)?);
        next = link.as_deref().and_then(parse_link_next);
    }
    Ok(out)
}

/// Replace the stored issues for one repo (delete-then-insert inside a
/// transaction) and upsert the `github_remotes` bookkeeping row. Full replace
/// so issues that were deleted/transferred upstream don't linger. Returns the
/// number of rows written.
pub fn persist_issues(
    conn: &Connection,
    remote_url: &str,
    owner: &str,
    repo: &str,
    issues: &[Issue],
) -> Result<usize, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin tx: {e}"))?;
    tx.execute(
        "INSERT INTO github_remotes (remote_url, owner, repo, last_refreshed)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(remote_url) DO UPDATE SET
             owner = excluded.owner, repo = excluded.repo,
             last_refreshed = excluded.last_refreshed",
        params![remote_url, owner, repo],
    )
    .map_err(|e| format!("upsert github_remotes: {e}"))?;
    tx.execute(
        "DELETE FROM github_issues WHERE remote_url = ?1",
        params![remote_url],
    )
    .map_err(|e| format!("clear github_issues: {e}"))?;
    let mut n = 0;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO github_issues
                 (remote_url, issue_number, title, state, author_login, assignees_csv,
                  labels_csv, is_pr, created_at, updated_at, closed_at, url)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT(remote_url, issue_number) DO UPDATE SET
                   title=excluded.title, state=excluded.state,
                   author_login=excluded.author_login, assignees_csv=excluded.assignees_csv,
                   labels_csv=excluded.labels_csv, is_pr=excluded.is_pr,
                   created_at=excluded.created_at, updated_at=excluded.updated_at,
                   closed_at=excluded.closed_at, url=excluded.url",
            )
            .map_err(|e| format!("prepare insert: {e}"))?;
        for i in issues {
            stmt.execute(params![
                i.remote_url,
                i.issue_number,
                i.title,
                i.state,
                i.author_login,
                i.assignees_csv,
                i.labels_csv,
                i.is_pr as i64,
                i.created_at,
                i.updated_at,
                i.closed_at,
                i.url,
            ])
            .map_err(|e| format!("insert issue #{}: {e}", i.issue_number))?;
            n += 1;
        }
    }
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(n)
}

/// Read all stored issues (excluding PRs) with their computed kanban lane,
/// for the `/api/issues` endpoint (the all-projects board).
pub fn list_issue_views(conn: &Connection) -> Result<Vec<IssueView>, String> {
    issue_views_query(
        conn,
        "SELECT remote_url, issue_number, title, state, author_login, assignees_csv,
                labels_csv, is_pr, created_at, updated_at, closed_at, url
         FROM github_issues WHERE is_pr = 0
         ORDER BY remote_url, issue_number DESC",
        [],
    )
}

/// Read the stored issues (excluding PRs) for one repo, open first, for the
/// per-project drill-down (`/api/project/issues`). `remote_url` must be the
/// canonical `https://github.com/owner/repo`.
pub fn list_issue_views_for_remote(
    conn: &Connection,
    remote_url: &str,
) -> Result<Vec<IssueView>, String> {
    issue_views_query(
        conn,
        "SELECT remote_url, issue_number, title, state, author_login, assignees_csv,
                labels_csv, is_pr, created_at, updated_at, closed_at, url
         FROM github_issues WHERE is_pr = 0 AND remote_url = ?1
         ORDER BY (state = 'closed'), issue_number DESC",
        [remote_url],
    )
}

fn issue_views_query<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<IssueView>, String> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("prepare issues: {e}"))?;
    let rows = stmt
        .query_map(params, Issue::from_row)
        .map_err(|e| format!("query issues: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        let issue = row.map_err(|e| format!("read issue: {e}"))?;
        let lane = issue_lane(&issue.state, issue.labels_csv.as_deref());
        out.push(IssueView { issue, lane });
    }
    Ok(out)
}

/// The GitHub-issues enrichment provider. Applies to any surveyed project
/// whose remote is on `github.com`; ownership scoping happens in `run`.
pub struct GithubIssuesEnrichment {
    pub token: Option<String>,
    /// Owner allowlist (lowercased). Empty = allow any owner.
    pub owners: Vec<String>,
    /// Require admin (owned) rather than push access.
    pub owned_only: bool,
}

impl GithubIssuesEnrichment {
    /// Whether a repo passes the ownership scope: owner allowlist (if any) +
    /// the permission bar (`owned_only` → admin, else push).
    fn in_scope(&self, owner: &str, meta: &RepoMeta) -> bool {
        let owner_ok =
            self.owners.is_empty() || self.owners.iter().any(|o| o == &owner.to_lowercase());
        let perm_ok = if self.owned_only {
            meta.is_admin()
        } else {
            meta.can_push()
        };
        owner_ok && perm_ok
    }
}

impl Enrichment for GithubIssuesEnrichment {
    fn name(&self) -> &'static str {
        "GitHubIssues"
    }

    fn applies_to(&self, p: &Project) -> bool {
        p.remote_url.as_deref().and_then(parse_owner_repo).is_some()
    }

    async fn run(
        &self,
        db_path: &Path,
        projects: &[Project],
    ) -> Result<EnrichmentSummary, EnrichmentError> {
        let mut summary = EnrichmentSummary::new(self.name());

        // Candidate repos: dedup by owner/repo. A local clone and its GitHub
        // twin resolve to the same repo; everything downstream keys on the
        // canonical `github.com/owner/repo`, so the two spellings collapse.
        let mut seen = std::collections::HashSet::new();
        let candidates: Vec<(String, String)> = projects
            .iter()
            .filter(|p| self.applies_to(p))
            .filter_map(|p| p.remote_url.as_deref())
            .filter_map(parse_owner_repo)
            .filter(|(o, r)| seen.insert(format!("{o}/{r}")))
            .collect();
        if candidates.is_empty() {
            return Ok(summary);
        }

        let token = self.token.as_deref();
        let client = reqwest::Client::new();
        let client = &client;

        // Wave 1 — repo metadata. Keep only repos the authenticated user can
        // push to ("my projects"), and among those only ones with Issues
        // enabled. This excludes read-only forks (cnych/…, cohnen/…) and skips
        // a wasted issues fetch (and 404) on issue-disabled repos.
        let metas = futures::future::join_all(
            candidates
                .iter()
                .map(|(o, r)| async move { (o, r, fetch_repo_meta(client, o, r, token).await) }),
        )
        .await;
        let mut owned: Vec<(&String, &String)> = Vec::new();
        for (o, r, res) in metas {
            match res {
                Ok(m) if self.in_scope(o, &m) => {
                    if m.has_issues {
                        owned.push((o, r));
                    }
                }
                Ok(_) => {} // out of scope (read-only, or not owned) — skip
                Err(EnrichmentError::Api { status: 404, .. }) => {} // gone / no access
                Err(e) => summary.errors.push(format!("{o}/{r} meta: {e}")),
            }
        }
        summary.projects_touched = owned.len();

        // Wave 2 — issues for owned repos only.
        let issue_results =
            futures::future::join_all(owned.iter().map(|&(o, r)| async move {
                (o, r, fetch_repo_issues(client, o, r, token).await)
            }))
            .await;

        let conn = crate::db::open(db_path).map_err(EnrichmentError::Config)?;
        for (o, r, res) in issue_results {
            // Persist under the canonical remote — the same URL
            // `fetch_repo_issues` stamps on each Issue, so DELETE-then-INSERT
            // agree across runs regardless of the project's remote spelling.
            let remote = format!("https://github.com/{o}/{r}");
            match res {
                Ok(issues) => match persist_issues(&conn, &remote, o, r, &issues) {
                    Ok(n) => summary.records_upserted += n,
                    Err(e) => summary.errors.push(format!("{o}/{r}: {e}")),
                },
                Err(e) => summary.errors.push(format!("{o}/{r}: {e}")),
            }
        }

        let ok = summary.errors.is_empty();
        let joined = summary.errors.join("; ");
        let _ = record_state(
            &conn,
            self.name(),
            "all",
            ok,
            if ok { None } else { Some(joined.as_str()) },
        );
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_repo_handles_https_ssh_and_git_suffix() {
        assert_eq!(
            parse_owner_repo("https://github.com/zot24/mercator"),
            Some(("zot24".into(), "mercator".into()))
        );
        assert_eq!(
            parse_owner_repo("git@github.com:zot24/mercator.git"),
            Some(("zot24".into(), "mercator".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/zot24/mercator/"),
            Some(("zot24".into(), "mercator".into()))
        );
        // Non-GitHub remotes are skipped.
        assert_eq!(parse_owner_repo("https://gitlab.com/zot24/thing"), None);
        assert_eq!(parse_owner_repo("https://github.com/zot24"), None);
    }

    #[test]
    fn issue_lane_maps_state_and_labels() {
        assert_eq!(issue_lane("closed", None), "done");
        assert_eq!(issue_lane("open", None), "todo");
        assert_eq!(issue_lane("open", Some("bug,status:doing")), "doing");
        assert_eq!(issue_lane("open", Some("In Progress")), "doing");
        assert_eq!(issue_lane("open", Some("status:done")), "done");
        // Closed always wins over any label.
        assert_eq!(issue_lane("closed", Some("status:doing")), "done");
    }

    #[test]
    fn issues_from_json_parses_labels_assignees_and_pr_flag() {
        let body = r#"[
            {"number":8,"title":"Deploy targets","state":"open","user":{"login":"zot24"},
             "assignees":[{"login":"zot24"},{"login":"motty"}],
             "labels":[{"name":"enhancement"},{"name":"status:doing"}],
             "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
             "closed_at":null,"html_url":"https://github.com/zot24/mercator/issues/8"},
            {"number":9,"title":"A PR","state":"open","user":{"login":"zot24"},
             "pull_request":{"url":"x"},
             "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
             "closed_at":null,"html_url":"https://github.com/zot24/mercator/pull/9"}
        ]"#;
        let issues = issues_from_json("https://github.com/zot24/mercator", body).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].issue_number, 8);
        assert_eq!(
            issues[0].labels_csv.as_deref(),
            Some("enhancement,status:doing")
        );
        assert_eq!(issues[0].assignees_csv.as_deref(), Some("zot24,motty"));
        assert!(!issues[0].is_pr);
        assert!(issues[1].is_pr, "the pull_request key marks a PR");
    }

    #[test]
    fn repo_meta_ownership_and_issues_flags() {
        // Owned repo (push) with issues enabled → kept.
        let mine: RepoMeta = serde_json::from_str(
            r#"{"has_issues":true,"permissions":{"admin":true,"push":true,"pull":true}}"#,
        )
        .unwrap();
        assert!(mine.can_push() && mine.has_issues);

        // Read-only fork (pull only) → not mine.
        let fork: RepoMeta = serde_json::from_str(
            r#"{"has_issues":true,"permissions":{"admin":false,"push":false,"pull":true}}"#,
        )
        .unwrap();
        assert!(!fork.can_push());

        // No permissions block (unauthenticated shape) → not mine.
        let anon: RepoMeta = serde_json::from_str(r#"{"has_issues":true}"#).unwrap();
        assert!(!anon.can_push());

        // has_issues defaults to true when GitHub omits it.
        let bare: RepoMeta = serde_json::from_str(r#"{"permissions":{"push":true}}"#).unwrap();
        assert!(bare.has_issues && bare.can_push());
    }

    #[test]
    fn in_scope_applies_owner_allowlist_and_permission_bar() {
        let admin: RepoMeta =
            serde_json::from_str(r#"{"permissions":{"admin":true,"push":true}}"#).unwrap();
        let collab: RepoMeta =
            serde_json::from_str(r#"{"permissions":{"admin":false,"push":true}}"#).unwrap();

        // Default: any pushable repo, no owner filter.
        let any = GithubIssuesEnrichment {
            token: None,
            owners: vec![],
            owned_only: false,
        };
        assert!(any.in_scope("destin18", &collab));
        assert!(any.in_scope("zot24", &admin));

        // owned_only drops the push-only collaboration but keeps admin repos.
        let owned = GithubIssuesEnrichment {
            token: None,
            owners: vec![],
            owned_only: true,
        };
        assert!(!owned.in_scope("destin18", &collab));
        assert!(owned.in_scope("zot24", &admin));

        // Owner allowlist (case-insensitive) excludes owners not listed.
        let listed = GithubIssuesEnrichment {
            token: None,
            owners: vec!["zot24".into(), "motty".into()],
            owned_only: false,
        };
        assert!(listed.in_scope("ZOT24", &admin));
        assert!(!listed.in_scope("destin18", &collab));
    }

    #[test]
    fn list_issue_views_for_remote_scopes_to_one_repo() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("db.sqlite")).unwrap();
        let mk = |ru: &str, num: i64, state: &str| {
            issues_from_json(
                ru,
                &format!(
                    r#"[{{"number":{num},"title":"t","state":"{state}","user":{{"login":"z"}},
                        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z",
                        "closed_at":null,"html_url":"{ru}/issues/{num}"}}]"#
                ),
            )
            .unwrap()
        };
        let a = "https://github.com/zot24/mercator";
        let b = "https://github.com/zot24/paraguayos";
        persist_issues(&conn, a, "zot24", "mercator", &mk(a, 1, "open")).unwrap();
        persist_issues(&conn, b, "zot24", "paraguayos", &mk(b, 2, "open")).unwrap();

        let only_a = list_issue_views_for_remote(&conn, a).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].issue.remote_url, a);
    }

    #[test]
    fn persist_tolerates_duplicate_issue_numbers_in_a_batch() {
        // GitHub pagination can repeat an item across page boundaries. A batch
        // with a duplicate issue_number must not abort the whole repo's tx —
        // ON CONFLICT updates to the last-seen row instead.
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("db.sqlite")).unwrap();
        let ru = "https://github.com/zot24/mercator";
        let mut a = issues_from_json(
            ru,
            r#"[{"number":8,"title":"first","state":"open","user":{"login":"z"},
                 "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z",
                 "closed_at":null,"html_url":"https://github.com/zot24/mercator/issues/8"}]"#,
        )
        .unwrap();
        let dup = issues_from_json(
            ru,
            r#"[{"number":8,"title":"second","state":"closed","user":{"login":"z"},
                 "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
                 "closed_at":"2026-01-02T00:00:00Z","html_url":"https://github.com/zot24/mercator/issues/8"}]"#,
        )
        .unwrap();
        a.extend(dup);
        // Two rows, same (remote_url, issue_number) — must not error.
        let n = persist_issues(&conn, ru, "zot24", "mercator", &a).unwrap();
        assert_eq!(n, 2);
        let views = list_issue_views(&conn).unwrap();
        assert_eq!(views.len(), 1, "one row survives the conflict");
        assert_eq!(views[0].issue.title, "second", "last write wins");
        assert_eq!(views[0].lane, "done");
    }

    #[test]
    fn persist_then_list_excludes_prs_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("db.sqlite")).unwrap();
        let ru = "https://github.com/zot24/mercator";
        let body = r#"[
            {"number":8,"title":"issue","state":"open","user":{"login":"zot24"},
             "labels":[{"name":"status:doing"}],
             "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
             "closed_at":null,"html_url":"https://github.com/zot24/mercator/issues/8"},
            {"number":9,"title":"pr","state":"open","user":{"login":"zot24"},"pull_request":{"url":"x"},
             "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
             "closed_at":null,"html_url":"https://github.com/zot24/mercator/pull/9"}
        ]"#;
        let issues = issues_from_json(ru, body).unwrap();
        let n = persist_issues(&conn, ru, "zot24", "mercator", &issues).unwrap();
        assert_eq!(n, 2, "both rows stored (issue + pr)");

        let views = list_issue_views(&conn).unwrap();
        assert_eq!(views.len(), 1, "PRs excluded from the kanban");
        assert_eq!(views[0].issue.issue_number, 8);
        assert_eq!(views[0].lane, "doing");

        // Re-persist with a smaller set → old rows for this remote are gone.
        let smaller = issues_from_json(
            ru,
            r#"[{"number":8,"title":"issue","state":"closed","user":{"login":"zot24"},
                 "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-03T00:00:00Z",
                 "closed_at":"2026-01-03T00:00:00Z","html_url":"https://github.com/zot24/mercator/issues/8"}]"#,
        )
        .unwrap();
        persist_issues(&conn, ru, "zot24", "mercator", &smaller).unwrap();
        let views = list_issue_views(&conn).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].lane, "done", "reflects the updated closed state");
    }
}
