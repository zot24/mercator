//! Vercel **deploy-status** ingestion — an [`Enrichment`](crate::enrichment)
//! provider that records the latest deployment per project so the dashboard
//! can badge each project card with its production deploy health (#8).
//!
//! Vercel's model is not GitHub's: a deployment has a `readyState`
//! (READY/ERROR/BUILDING/…), a `target` (production/preview), and a git
//! commit — so it gets its **own** `vercel_deployments` table (schema v6)
//! rather than reusing the GitHub-shaped scaffold.
//!
//! ## Join to local projects
//!
//! Mercator keys everything on a project's git remote. A Vercel project
//! carries its linked git repo (`link.org`/`link.repo`, or the deployment's
//! `meta.githubCommit*`), which we turn into a `github.com/owner/repo` URL and
//! match against surveyed projects via [`normalize_remote_url`]. On a match we
//! store the deployment under that project's own `remote_url`, so the
//! dashboard can join by exact string.
//!
//! One page of `GET /v9/projects` includes each project's `latestDeployments`,
//! so a whole account's deploy status is one request — no per-project fan-out.

use crate::enrichment::{record_state, Enrichment, EnrichmentError, EnrichmentSummary};
use crate::project::Project;
use crate::sources::normalize_remote_url;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;

/// A Vercel deployment as stored in `vercel_deployments`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VercelDeployment {
    pub uid: String,
    #[serde(rename = "remoteUrl")]
    pub remote_url: Option<String>,
    #[serde(rename = "projectName")]
    pub project_name: String,
    /// READY / ERROR / BUILDING / QUEUED / CANCELED / … (uppercased).
    pub state: String,
    pub target: Option<String>,
    pub url: Option<String>,
    pub branch: Option<String>,
    #[serde(rename = "commitSha")]
    pub commit_sha: Option<String>,
    #[serde(rename = "commitMessage")]
    pub commit_message: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    #[serde(rename = "readyAt")]
    pub ready_at: Option<String>,
    #[serde(rename = "inspectorUrl")]
    pub inspector_url: Option<String>,
}

impl VercelDeployment {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            uid: r.get(0)?,
            remote_url: r.get(1)?,
            project_name: r.get(2)?,
            state: r.get(3)?,
            target: r.get(4)?,
            url: r.get(5)?,
            branch: r.get(6)?,
            commit_sha: r.get(7)?,
            commit_message: r.get(8)?,
            created_at: r.get(9)?,
            ready_at: r.get(10)?,
            inspector_url: r.get(11)?,
        })
    }
}

// ── raw API shapes (tolerant: field names differ between the /v9/projects
//    latestDeployments entries and the /v6/deployments list) ──────────────

#[derive(serde::Deserialize)]
struct ProjectsPage {
    #[serde(default)]
    projects: Vec<VProject>,
    #[serde(default)]
    pagination: Pagination,
}

#[derive(serde::Deserialize, Default)]
struct Pagination {
    /// ms epoch cursor for the next (older) page, or null when exhausted.
    next: Option<i64>,
}

#[derive(serde::Deserialize)]
struct VProject {
    name: String,
    #[serde(default)]
    link: Option<VLink>,
    #[serde(default, rename = "latestDeployments")]
    latest_deployments: Vec<VDeploy>,
}

#[derive(serde::Deserialize)]
struct VLink {
    org: Option<String>,
    repo: Option<String>,
}

#[derive(serde::Deserialize)]
struct VDeploy {
    #[serde(alias = "uid")]
    id: Option<String>,
    url: Option<String>,
    #[serde(rename = "readyState", alias = "state")]
    ready_state: Option<String>,
    target: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<i64>,
    ready: Option<i64>,
    #[serde(rename = "inspectorUrl")]
    inspector_url: Option<String>,
    #[serde(default)]
    meta: Option<VMeta>,
}

#[derive(serde::Deserialize, Default)]
struct VMeta {
    #[serde(rename = "githubCommitRef")]
    github_commit_ref: Option<String>,
    #[serde(rename = "githubCommitSha")]
    github_commit_sha: Option<String>,
    #[serde(rename = "githubCommitMessage")]
    github_commit_message: Option<String>,
    #[serde(rename = "githubCommitOrg")]
    github_commit_org: Option<String>,
    #[serde(rename = "githubCommitRepo")]
    github_commit_repo: Option<String>,
}

fn ms_to_iso(ms: Option<i64>) -> Option<String> {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.to_rfc3339())
}

/// Derive the `github.com/owner/repo` URL a Vercel project deploys from,
/// preferring the project link and falling back to the deployment commit meta.
/// Pure.
fn project_github_url(p: &VProject, latest: Option<&VDeploy>) -> Option<String> {
    if let Some(l) = &p.link {
        if let (Some(org), Some(repo)) = (l.org.as_deref(), l.repo.as_deref()) {
            if !org.is_empty() && !repo.is_empty() {
                return Some(format!("https://github.com/{org}/{repo}"));
            }
        }
    }
    if let Some(m) = latest.and_then(|d| d.meta.as_ref()) {
        if let (Some(org), Some(repo)) = (
            m.github_commit_org.as_deref(),
            m.github_commit_repo.as_deref(),
        ) {
            if !org.is_empty() && !repo.is_empty() {
                return Some(format!("https://github.com/{org}/{repo}"));
            }
        }
    }
    None
}

/// Turn one page of `/v9/projects` JSON into `(remote_url_matched, deployment)`
/// pairs. `remote_by_norm` maps a normalized git remote to the surveyed
/// project's original `remote_url`, so a match stores the deployment under the
/// project's own key (exact-string joinable in the dashboard). Pure — no I/O.
pub fn deployments_from_projects(
    body: &str,
    remote_by_norm: &HashMap<String, String>,
) -> Result<Vec<VercelDeployment>, EnrichmentError> {
    let page: ProjectsPage =
        serde_json::from_str(body).map_err(|e| EnrichmentError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    for p in &page.projects {
        // Prefer the production deployment; else the most recent listed.
        let latest = p
            .latest_deployments
            .iter()
            .find(|d| d.target.as_deref() == Some("production"))
            .or_else(|| p.latest_deployments.first());
        let Some(d) = latest else { continue };
        let Some(uid) = d.id.clone() else { continue };

        let gh = project_github_url(p, Some(d));
        let remote_url = gh
            .as_deref()
            .map(normalize_remote_url)
            .and_then(|n| remote_by_norm.get(&n).cloned())
            .or(gh);

        let meta = d.meta.as_ref();
        out.push(VercelDeployment {
            uid,
            remote_url,
            project_name: p.name.clone(),
            state: d
                .ready_state
                .clone()
                .unwrap_or_else(|| "UNKNOWN".into())
                .to_uppercase(),
            target: d.target.clone(),
            url: d.url.as_ref().map(|u| {
                if u.starts_with("http") {
                    u.clone()
                } else {
                    format!("https://{u}")
                }
            }),
            branch: meta.and_then(|m| m.github_commit_ref.clone()),
            commit_sha: meta.and_then(|m| m.github_commit_sha.clone()),
            commit_message: meta.and_then(|m| m.github_commit_message.clone()),
            created_at: ms_to_iso(d.created_at),
            ready_at: ms_to_iso(d.ready),
            inspector_url: d.inspector_url.clone(),
        });
    }
    // `pagination.next` is consulted by the fetch loop, not here.
    let _ = page.pagination.next;
    Ok(out)
}

async fn fetch_all_projects(
    client: &reqwest::Client,
    token: &str,
    team: Option<&str>,
    remote_by_norm: &HashMap<String, String>,
) -> Result<Vec<VercelDeployment>, EnrichmentError> {
    let mut out = Vec::new();
    let mut until: Option<i64> = None;
    loop {
        let mut url = "https://api.vercel.com/v9/projects?limit=100".to_string();
        if let Some(t) = team {
            url.push_str(&format!("&teamId={t}"));
        }
        if let Some(u) = until {
            url.push_str(&format!("&until={u}"));
        }
        let resp = client
            .get(&url)
            .header("User-Agent", "Mercator/1.0")
            .header("Authorization", format!("Bearer {token}"))
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
        out.extend(deployments_from_projects(&body, remote_by_norm)?);

        // Advance the cursor if the API says there's an older page.
        let page: ProjectsPage =
            serde_json::from_str(&body).map_err(|e| EnrichmentError::Parse(e.to_string()))?;
        match page.pagination.next {
            Some(n) if Some(n) != until => until = Some(n),
            _ => break,
        }
    }
    Ok(out)
}

/// Replace the stored deployment for each project (delete-then-insert by
/// `remote_url`, keyed by uid otherwise) so exactly the latest survives.
pub fn persist_deployments(
    conn: &Connection,
    deployments: &[VercelDeployment],
) -> Result<usize, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin tx: {e}"))?;
    let mut n = 0;
    for d in deployments {
        // Keep one row per project. Clear by remote_url when known (the join
        // key the dashboard uses); otherwise clear by uid.
        if let Some(ru) = &d.remote_url {
            tx.execute(
                "DELETE FROM vercel_deployments WHERE remote_url = ?1",
                params![ru],
            )
            .map_err(|e| format!("clear by remote: {e}"))?;
        }
        tx.execute(
            "INSERT INTO vercel_deployments
             (uid, remote_url, project_name, state, target, url, branch,
              commit_sha, commit_message, created_at, ready_at, inspector_url)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(uid) DO UPDATE SET
               remote_url=excluded.remote_url, project_name=excluded.project_name,
               state=excluded.state, target=excluded.target, url=excluded.url,
               branch=excluded.branch, commit_sha=excluded.commit_sha,
               commit_message=excluded.commit_message, created_at=excluded.created_at,
               ready_at=excluded.ready_at, inspector_url=excluded.inspector_url",
            params![
                d.uid,
                d.remote_url,
                d.project_name,
                d.state,
                d.target,
                d.url,
                d.branch,
                d.commit_sha,
                d.commit_message,
                d.created_at,
                d.ready_at,
                d.inspector_url,
            ],
        )
        .map_err(|e| format!("insert deployment {}: {e}", d.uid))?;
        n += 1;
    }
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(n)
}

/// Read all stored deployments, latest-per-project, for `/api/deployments`.
pub fn list_deployments(conn: &Connection) -> Result<Vec<VercelDeployment>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT uid, remote_url, project_name, state, target, url, branch,
                    commit_sha, commit_message, created_at, ready_at, inspector_url
             FROM vercel_deployments ORDER BY project_name",
        )
        .map_err(|e| format!("prepare list deployments: {e}"))?;
    let rows = stmt
        .query_map([], VercelDeployment::from_row)
        .map_err(|e| format!("query deployments: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read deployments: {e}"))
}

/// The Vercel deploy-status enrichment provider.
pub struct VercelEnrichment {
    pub token: String,
    /// Optional `teamId` for team-scoped Vercel accounts.
    pub team: Option<String>,
}

impl Enrichment for VercelEnrichment {
    fn name(&self) -> &'static str {
        "Vercel"
    }

    fn applies_to(&self, p: &Project) -> bool {
        p.remote_url
            .as_deref()
            .map(normalize_remote_url)
            .map(|n| n.starts_with("github.com/"))
            .unwrap_or(false)
    }

    async fn run(
        &self,
        db_path: &Path,
        projects: &[Project],
    ) -> Result<EnrichmentSummary, EnrichmentError> {
        let mut summary = EnrichmentSummary::new(self.name());

        // Map normalized git remote → the project's original remote_url, so a
        // matched Vercel project stores its deployment under the exact key the
        // dashboard joins on.
        let remote_by_norm: HashMap<String, String> = projects
            .iter()
            .filter(|p| self.applies_to(p))
            .filter_map(|p| {
                p.remote_url
                    .as_deref()
                    .map(|ru| (normalize_remote_url(ru), ru.to_string()))
            })
            .collect();

        let client = reqwest::Client::new();
        let deployments =
            fetch_all_projects(&client, &self.token, self.team.as_deref(), &remote_by_norm).await?;
        summary.projects_touched = deployments
            .iter()
            .filter(|d| d.remote_url.is_some())
            .count();

        let conn = crate::db::open(db_path).map_err(EnrichmentError::Config)?;
        match persist_deployments(&conn, &deployments) {
            Ok(n) => summary.records_upserted = n,
            Err(e) => summary.errors.push(e),
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

    fn remote_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect()
    }

    #[test]
    fn parses_and_matches_production_deployment() {
        // paraguayos is surveyed (ssh remote form); mercator is not surveyed.
        let map = remote_map(&[(
            "github.com/zot24/paraguayos",
            "git@github.com:zot24/paraguayos.git",
        )]);
        let body = r#"{
          "projects": [
            {"name":"paraguayos","link":{"org":"zot24","repo":"paraguayos"},
             "latestDeployments":[
               {"uid":"dpl_prev","url":"prev.vercel.app","readyState":"READY","target":"preview","createdAt":1000},
               {"uid":"dpl_prod","url":"paraguayos.vercel.app","readyState":"ERROR","target":"production","createdAt":2000,"ready":2500,
                "inspectorUrl":"https://vercel.com/zot24/paraguayos/dpl_prod",
                "meta":{"githubCommitRef":"main","githubCommitSha":"abc123","githubCommitMessage":"broke it"}}
             ]},
            {"name":"unsurveyed","link":{"org":"zot24","repo":"mercator"},
             "latestDeployments":[{"uid":"dpl_m","url":"m.vercel.app","readyState":"READY","target":"production","createdAt":3000}]}
          ],
          "pagination": {"next": null}
        }"#;
        let deps = deployments_from_projects(body, &map).unwrap();
        assert_eq!(deps.len(), 2);

        let pgy = deps
            .iter()
            .find(|d| d.project_name == "paraguayos")
            .unwrap();
        // Production chosen over the earlier preview.
        assert_eq!(pgy.uid, "dpl_prod");
        assert_eq!(pgy.state, "ERROR");
        assert_eq!(pgy.target.as_deref(), Some("production"));
        // Stored under the surveyed project's ORIGINAL remote (join key).
        assert_eq!(
            pgy.remote_url.as_deref(),
            Some("git@github.com:zot24/paraguayos.git")
        );
        assert_eq!(pgy.branch.as_deref(), Some("main"));
        assert_eq!(pgy.url.as_deref(), Some("https://paraguayos.vercel.app"));
        assert!(pgy.created_at.as_deref().unwrap().contains('T'));

        // Unmatched project falls back to the derived github URL.
        let m = deps
            .iter()
            .find(|d| d.project_name == "unsurveyed")
            .unwrap();
        assert_eq!(
            m.remote_url.as_deref(),
            Some("https://github.com/zot24/mercator")
        );
    }

    #[test]
    fn accepts_v6_style_state_and_uid_aliases() {
        let map = HashMap::new();
        let body = r#"{"projects":[
          {"name":"x","link":{"org":"o","repo":"r"},
           "latestDeployments":[{"id":"dpl_1","url":"x.vercel.app","state":"BUILDING","target":"production","createdAt":1}]}
        ]}"#;
        let deps = deployments_from_projects(body, &map).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].uid, "dpl_1");
        assert_eq!(deps[0].state, "BUILDING");
    }

    #[test]
    fn persist_then_list_keeps_one_row_per_remote() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("db.sqlite")).unwrap();
        let mk = |uid: &str, state: &str| VercelDeployment {
            uid: uid.into(),
            remote_url: Some("https://github.com/zot24/paraguayos".into()),
            project_name: "paraguayos".into(),
            state: state.into(),
            target: Some("production".into()),
            url: None,
            branch: None,
            commit_sha: None,
            commit_message: None,
            created_at: None,
            ready_at: None,
            inspector_url: None,
        };
        persist_deployments(&conn, &[mk("dpl_old", "READY")]).unwrap();
        persist_deployments(&conn, &[mk("dpl_new", "ERROR")]).unwrap();
        let rows = list_deployments(&conn).unwrap();
        assert_eq!(rows.len(), 1, "delete-by-remote keeps one row per project");
        assert_eq!(rows[0].uid, "dpl_new");
        assert_eq!(rows[0].state, "ERROR");
    }
}
