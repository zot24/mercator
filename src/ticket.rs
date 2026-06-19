use crate::AppState;
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct TicketOut {
    pub id: Option<i64>,
    pub source: String,
    pub repo: Option<String>,
    pub project: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub priority: String,
    pub status: String,
    pub labels: Option<Vec<String>>,
    pub assignee: Option<String>,
    pub external_key: Option<String>,
    pub url: Option<String>,
    pub created_at: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TicketCreate {
    pub source: String,
    pub repo: Option<String>,
    pub project: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub priority: Option<String>,
    pub labels: Option<Vec<String>>,
    pub assignee: Option<String>,
}

impl TicketCreate {
    fn normalize(mut self) -> Self {
        self.title = self.title.trim().to_string();
        self.source = self.source.to_lowercase().trim().to_string();
        self.priority = self.priority.map(|p| p.to_lowercase().trim().to_string());
        self.labels = self.labels.map(|v| {
            v.into_iter()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        });
        self
    }
}

pub async fn create_ticket(
    State(state): State<AppState>,
    Json(body): Json<TicketCreate>,
) -> (StatusCode, Json<TicketOut>) {
    let body = body.normalize();

    if body.title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(TicketOut {
                id: None,
                source: body.source,
                repo: body.repo,
                project: body.project,
                title: body.title,
                body: body.body,
                priority: body.priority.unwrap_or_else(|| "medium".into()),
                status: "open".into(),
                labels: body.labels,
                assignee: body.assignee,
                external_key: None,
                url: None,
                created_at: None,
                note: Some("title required".into()),
            }),
        );
    }

    let priority = body.priority.clone().unwrap_or_else(|| "medium".into());

    match body.source.as_str() {
        "github" => {
            let repo = match body.repo.as_ref() {
                Some(r) if !r.trim().is_empty() => r.trim(),
                _ => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(TicketOut {
                            id: None,
                            source: body.source,
                            repo: body.repo,
                            project: body.project,
                            title: body.title,
                            body: body.body,
                            priority,
                            status: "open".into(),
                            labels: body.labels,
                            assignee: body.assignee,
                            external_key: None,
                            url: None,
                            created_at: None,
                            note: Some("github requires repo".into()),
                        }),
                    );
                }
            };
            let github_token = state.cfg.lock().await.github.token().map(str::to_string);
            match create_github_issue(
                github_token.as_deref(),
                repo,
                &body.title,
                body.body.as_deref(),
                body.labels.clone(),
                body.assignee.as_deref(),
            )
            .await
            {
                Ok((number, html_url)) => (
                    StatusCode::CREATED,
                    Json(TicketOut {
                        id: None,
                        source: "github".into(),
                        repo: Some(repo.into()),
                        project: body.project,
                        title: body.title,
                        body: body.body,
                        priority,
                        status: "open".into(),
                        labels: body.labels,
                        assignee: body.assignee,
                        external_key: Some(number.to_string()),
                        url: Some(html_url),
                        created_at: Some(now_rfc3339()),
                        note: None,
                    }),
                ),
                Err(err) => (
                    StatusCode::BAD_GATEWAY,
                    Json(TicketOut {
                        id: None,
                        source: "github".into(),
                        repo: Some(repo.into()),
                        project: body.project,
                        title: body.title,
                        body: body.body,
                        priority,
                        status: "open".into(),
                        labels: body.labels,
                        assignee: body.assignee,
                        external_key: None,
                        url: None,
                        created_at: None,
                        note: Some(err),
                    }),
                ),
            }
        }
        "local" => {
            let insert = {
                let conn = state.db.lock().await;
                insert_local_ticket(&conn, &body, &priority)
            };
            match insert {
                Ok(id) => (
                    StatusCode::CREATED,
                    Json(TicketOut {
                        id: Some(id),
                        source: "local".into(),
                        repo: body.repo,
                        project: body.project,
                        title: body.title,
                        body: body.body,
                        priority,
                        status: "open".into(),
                        labels: body.labels,
                        assignee: body.assignee,
                        external_key: None,
                        url: None,
                        created_at: Some(now_rfc3339()),
                        note: None,
                    }),
                ),
                Err(err) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(TicketOut {
                        id: None,
                        source: "local".into(),
                        repo: body.repo,
                        project: body.project,
                        title: body.title,
                        body: body.body,
                        priority,
                        status: "open".into(),
                        labels: body.labels,
                        assignee: body.assignee,
                        external_key: None,
                        url: None,
                        created_at: None,
                        note: Some(err),
                    }),
                ),
            }
        }
        other => (
            StatusCode::BAD_REQUEST,
            Json(TicketOut {
                id: None,
                source: other.to_string(),
                repo: body.repo,
                project: body.project,
                title: body.title,
                body: body.body,
                priority,
                status: "open".into(),
                labels: body.labels,
                assignee: body.assignee,
                external_key: None,
                url: None,
                created_at: None,
                note: Some("source must be github or local".into()),
            }),
        ),
    }
}

async fn create_github_issue(
    token: Option<&str>,
    repo: &str,
    title: &str,
    body: Option<&str>,
    labels: Option<Vec<String>>,
    _assignee: Option<&str>,
) -> Result<(i64, String), String> {
    let token = token.ok_or("missing github token")?;
    let http = reqwest::Client::new();
    let url = format!("https://api.github.com/repos/{}/issues", repo);

    let mut payload = serde_json::json!({
        "title": title,
    });
    if let Some(b) = body {
        payload["body"] = serde_json::Value::String(b.into());
    }
    if let Some(l) = labels {
        if !l.is_empty() {
            payload["labels"] =
                serde_json::Value::Array(l.into_iter().map(serde_json::Value::String).collect());
        }
    }

    let res = http
        .post(&url)
        .bearer_auth(token)
        .header(reqwest::header::USER_AGENT, "mercator")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    if !status.is_success() {
        return Err(format!("github error {status}: {text}"));
    }

    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("json parse failed: {e}: {text}"))?;
    let number = value["number"]
        .as_i64()
        .ok_or_else(|| format!("missing number in response: {text}"))?;
    let html_url = value["html_url"].as_str().unwrap_or("").to_string();

    if html_url.is_empty() {
        return Err(format!("missing html_url in response: {text}"));
    }

    Ok((number, html_url))
}

fn insert_local_ticket(
    conn: &rusqlite::Connection,
    body: &TicketCreate,
    priority: &str,
) -> Result<i64, String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS local_tickets (\
         id INTEGER PRIMARY KEY AUTOINCREMENT,\
         source TEXT NOT NULL,\
         repo TEXT,\
         project TEXT,\
         title TEXT NOT NULL,\
         body TEXT,\
         priority TEXT NOT NULL DEFAULT 'medium',\
         status TEXT NOT NULL DEFAULT 'open',\
         labels_csv TEXT,\
         assignee TEXT,\
         external_key TEXT,\
         created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),\
         updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),\
         closed_at TEXT\
         )",
    )
    .map_err(|e| format!("create table failed: {e}"))?;

    let labels_csv = body.labels.as_ref().map(|v| v.join(","));
    conn.execute(
        "INSERT INTO local_tickets (source, repo, project, title, body, priority, status, labels_csv, assignee) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8)",
        rusqlite::params![
            body.source,
            body.repo,
            body.project,
            body.title,
            body.body,
            priority,
            labels_csv,
            body.assignee,
        ],
    )
    .map_err(|e| format!("insert failed: {e}"))?;

    Ok(conn.last_insert_rowid())
}

fn now_rfc3339() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = dur.as_nanos();
    let secs = nanos / 1_000_000_000;
    let rem = nanos % 1_000_000_000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        (secs / 31_536_000) + 1970,
        ((secs % 31_536_000) / 2_629_746) + 1, // month approx
        ((secs % 2_629_746) / 86_400) + 1,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60,
        rem,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_title() {
        let t = TicketCreate {
            source: " GitHub ".into(),
            repo: Some("o/r".into()),
            project: None,
            title: "  hi ".into(),
            body: None,
            priority: Some(" HIGH ".into()),
            labels: Some(vec![" bug ".into(), "".into()]),
            assignee: None,
        }
        .normalize();
        assert_eq!(t.title, "hi");
        assert_eq!(t.source, "github");
        assert_eq!(t.priority, Some("high".into()));
        assert_eq!(t.labels, Some(vec!["bug".into()]));
    }
}
