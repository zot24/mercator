//! The **enrichment** plug-point — the second extension axis after `Source`.
//!
//! `Source` (see [`crate::sources`]) discovers *projects*: its `fetch`
//! returns `Vec<Project>`. Enrichment is the orthogonal concern of attaching
//! *per-project child data* — GitHub issues, Vercel deployments, and (later)
//! Supabase/Turso status — to projects that already exist. That data can't
//! ride the `Source` trait because it isn't a project; it hangs off one.
//!
//! ## Shape
//!
//! An [`Enrichment`] provider:
//! 1. declares which projects it [`applies_to`](Enrichment::applies_to)
//!    (usually by inspecting `Project::remote_url`),
//! 2. fetches child records over the network for those projects, and
//! 3. persists them into its **own** table(s) via a fresh connection it
//!    opens from the DB path (WAL + `busy_timeout` make this safe to do
//!    concurrently with the `serve`/`survey` connection).
//!
//! Providers are dispatched through an `AnyEnrichment` enum (added alongside
//! the first provider) exactly like `AnySource` — see
//! [`docs/decisions/0003-source-trait-enum-dispatch.md`] for why enum over
//! `Box<dyn>`. Adding a backend = one struct + one `impl Enrichment` + one
//! `AnyEnrichment` variant + one table.
//!
//! ## Bookkeeping
//!
//! Every provider run records a row in `enrichment_state` (schema v6) via
//! [`record_state`], so the dashboard can show "last synced / errored" per
//! provider without the provider having to expose its own status surface.

use crate::project::Project;
use rusqlite::{params, Connection};
use std::path::Path;

/// A structured enrichment failure. Splits the old `SourceError::Generic`
/// into the variants deploy/API integrations actually need to discriminate
/// (STATUS.md #8): transport vs. HTTP-status vs. body-parse vs. local
/// misconfiguration. `Display` renders a single human line for logs and the
/// `enrichment_state.error` column.
#[derive(Debug)]
pub enum EnrichmentError {
    /// Missing/blank token or other local misconfiguration — not the API's fault.
    Config(String),
    /// Transport failure (DNS, TLS, connection reset) before a response.
    Network(String),
    /// The API responded with a non-success status.
    Api { status: u16, body: String },
    /// A response arrived but couldn't be parsed into the expected shape.
    Parse(String),
}

impl std::fmt::Display for EnrichmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnrichmentError::Config(s) => write!(f, "config: {s}"),
            EnrichmentError::Network(s) => write!(f, "network: {s}"),
            EnrichmentError::Api { status, body } => write!(f, "api {status}: {body}"),
            EnrichmentError::Parse(s) => write!(f, "parse: {s}"),
        }
    }
}

impl std::error::Error for EnrichmentError {}

/// What a single provider run accomplished. Aggregated by [`run_all`] and
/// surfaced (as JSON) by the dashboard refresh handler.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnrichmentSummary {
    pub provider: &'static str,
    /// Projects this provider matched and attempted to enrich.
    pub projects_touched: usize,
    /// Child records written (issues, deployments, …).
    pub records_upserted: usize,
    /// Per-project (or per-repo) errors that didn't abort the whole run.
    pub errors: Vec<String>,
}

impl EnrichmentSummary {
    pub fn new(provider: &'static str) -> Self {
        Self {
            provider,
            projects_touched: 0,
            records_upserted: 0,
            errors: Vec::new(),
        }
    }
}

/// A per-project child-data provider. See the module docs for the contract.
///
/// `run` takes the **DB path** rather than a shared connection so it can do
/// its network I/O without holding any lock, then open a short-lived write
/// connection at the end — uniform across the CLI (`survey`, owns a plain
/// `Connection`) and the dashboard (`serve`, holds an `Arc<Mutex<_>>`).
pub trait Enrichment {
    /// Short stable identifier used in logs and `enrichment_state.provider`.
    fn name(&self) -> &'static str;

    /// Whether this provider has anything to fetch for `p` (typically a
    /// `remote_url` host/shape check). Pure and cheap; used to compute
    /// `projects_touched` and to skip irrelevant projects.
    fn applies_to(&self, p: &Project) -> bool;

    /// Fetch + persist child data for the applicable projects.
    fn run(
        &self,
        db_path: &Path,
        projects: &[Project],
    ) -> impl std::future::Future<Output = Result<EnrichmentSummary, EnrichmentError>> + Send;
}

/// Upsert a provider's sync status. `scope` lets a provider record either a
/// single roll-up row (`"all"`) or per-repo rows (a `remote_url`). Timestamp
/// is minted by SQLite in the same ISO-ish shape as `projects.last_seen`.
pub fn record_state(
    conn: &Connection,
    provider: &str,
    scope: &str,
    ok: bool,
    error: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO enrichment_state (provider, scope, last_refreshed, ok, error)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3, ?4)
         ON CONFLICT(provider, scope) DO UPDATE SET
             last_refreshed = excluded.last_refreshed,
             ok = excluded.ok,
             error = excluded.error",
        params![provider, scope, ok as i64, error],
    )
    .map(|_| ())
    .map_err(|e| format!("record enrichment_state: {e}"))
}

/// Enum dispatch over all enrichment providers — the enrichment analogue of
/// `sources::AnySource`. A new backend adds one variant here plus arms below.
pub enum AnyEnrichment {
    GithubIssues(crate::github::GithubIssuesEnrichment),
}

impl AnyEnrichment {
    pub fn name(&self) -> &'static str {
        match self {
            AnyEnrichment::GithubIssues(e) => e.name(),
        }
    }

    pub async fn run(
        &self,
        db_path: &Path,
        projects: &[Project],
    ) -> Result<EnrichmentSummary, EnrichmentError> {
        match self {
            AnyEnrichment::GithubIssues(e) => e.run(db_path, projects).await,
        }
    }
}

/// Build the enrichment provider list from available credentials. A provider
/// is only included when it has what it needs to run without immediately
/// tripping rate limits (GitHub issue ingestion needs a token — the same one
/// the GitHub source uses). Mirrors how the two dispatch sites build
/// `Vec<AnySource>` from config.
pub fn build_enrichments(github_token: Option<&str>) -> Vec<AnyEnrichment> {
    let mut v = Vec::new();
    if let Some(t) = github_token {
        v.push(AnyEnrichment::GithubIssues(
            crate::github::GithubIssuesEnrichment {
                token: Some(t.to_string()),
            },
        ));
    }
    v
}

/// Run every provider concurrently and collect one summary each. Errors that
/// abort a whole provider are folded into that provider's summary rather than
/// failing the batch — one broken integration must not sink the others.
/// Concurrency mirrors `survey`'s `join_all` over sources.
pub async fn run_all(
    db_path: &Path,
    projects: &[Project],
    providers: &[AnyEnrichment],
) -> Vec<EnrichmentSummary> {
    let results =
        futures::future::join_all(providers.iter().map(|p| p.run(db_path, projects))).await;
    providers
        .iter()
        .zip(results)
        .map(|(p, res)| match res {
            Ok(s) => s,
            Err(e) => {
                let mut s = EnrichmentSummary::new(p.name());
                s.errors.push(e.to_string());
                s
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_is_one_line_per_variant() {
        assert_eq!(
            EnrichmentError::Config("no token".into()).to_string(),
            "config: no token"
        );
        assert_eq!(
            EnrichmentError::Network("connection reset".into()).to_string(),
            "network: connection reset"
        );
        assert_eq!(
            EnrichmentError::Api {
                status: 401,
                body: "Bad credentials".into()
            }
            .to_string(),
            "api 401: Bad credentials"
        );
        assert_eq!(
            EnrichmentError::Parse("expected array".into()).to_string(),
            "parse: expected array"
        );
    }

    #[test]
    fn record_state_upserts_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("db.sqlite")).unwrap();

        record_state(&conn, "Vercel", "all", true, None).unwrap();
        // Same (provider, scope) again with an error flips ok + sets error.
        record_state(&conn, "Vercel", "all", false, Some("api 500")).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM enrichment_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "upsert must not duplicate the (provider, scope) row"
        );

        let (ok, err, refreshed): (i64, Option<String>, String) = conn
            .query_row(
                "SELECT ok, error, last_refreshed FROM enrichment_state WHERE provider = 'Vercel' AND scope = 'all'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(ok, 0, "second run flipped ok to false");
        assert_eq!(err.as_deref(), Some("api 500"));
        assert!(refreshed.ends_with('Z'));
    }
}
