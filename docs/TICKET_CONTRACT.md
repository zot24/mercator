# Mercator Ticket Contract

Add a standardized ticket/issue creation path to Mercator.

Mercator already surveys GitHub issues/PRs and stores them. This spec is for **creating** tickets from a standard shape, not replacing GitHub issues, but it should be usable as a GitHub issue creation backend too.

## Supported targets

- `github`: create a GitHub issue via the existing GitHub client/config (`github_token`, `gitlab_user`)
- `local`: create a ticket row in a new `tickets` table in Mercator DB

## Ticket contract

```rust
pub struct TicketCreate {
  pub source: TicketSource,           // github | local
  pub repo: String,                   // owner/repo for github; project key for local
  pub title: String,
  pub body: Option<String>,
  pub priority: Option<TicketPriority>, // low | medium | high | urgent
  pub labels: Option<Vec<String>>,
  pub project: Option<String>,        // Mercator project path or slug
  pub assignee: Option<String>,       // GitHub login or local user id
  pub external_key: Option<String>,   // optional issue number when syncing back
}

pub enum TicketSource {
  GitHub,
  Local,
}

pub enum TicketPriority {
  Low,
  Medium,
  High,
  Urgent,
}
```

## New endpoint

`POST /api/tickets`

- Auth: same `MERCATOR_TOKEN` as other `/api/*`
- Body: JSON matching `TicketCreate`
- Response: `201 Created` with serialized ticket record including assigned id and source metadata
- Errors:
  - 400 if `title` is empty or `source` is missing
  - 422 if GitHub target is selected but `repo` is missing/not configured
  - 502 if GitHub creation succeeds at the API level but response cannot be parsed

## New table: `local_tickets`

```sql
CREATE TABLE IF NOT EXISTS local_tickets (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  title       TEXT NOT NULL,
  body        TEXT,
  priority    TEXT NOT NULL DEFAULT 'medium',
  status      TEXT NOT NULL DEFAULT 'open',
  labels_csv  TEXT,
  repo        TEXT,
  project     TEXT,
  assignee    TEXT,
  source      TEXT NOT NULL,
  external_key TEXT,
  created_at  TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
  closed_at   TEXT
);
```

Fields enforced in Rust before insert:
- `title`: non-empty, trimmed
- `source`: normalized to lowercase
- `priority`: normalized from enum; stored as lowercase string
- `labels`: stored as comma-separated string

## GitHub issue creation path

Reuse `github.rs` request flow. Minimum needed:
- authenticated POST to `https://api.github.com/repos/{owner}/{repo}/issues`
- body fields: `title`, `body`, `labels`
- response: store returned `html_url` and `number` in `external_key`
- failure modes: missing token, wrong repo format, API error

## New route registration

Append to the existing `api = Router::new()` block in `main.rs`:

```rust
.route("/api/tickets", post(create_ticket))
```

## New module

Create `src/ticket.rs`:
- `TicketCreate`, `TicketSource`, `TicketPriority`
- `create_ticket()` handler
- Local insert helper
- GitHub create helper

This matches the existing split used by `db.rs`, `github.rs`, `project.rs`.

## Validation rules

- require at least `title` and `source`
- if `source == github`, `repo` must match `owner/repo`
- if `source == local`, `project` must be present or defaulted to active project from `active-projects.json`
- `labels` max 10 for GitHub path (API limit); local path has no hard limit
- idempotency: a ticket with identical `source+repo+title` within 60 seconds should return existing ticket instead of duplicate

## Consumer behavior

- Hermes repo watcher/triage should be able to call `POST /api/tickets` instead of `gh issue create`
- Dashboard should be able to list tickets alongside existing issues/PRs
- `mercator list --active` should surface ticket counts
- `active-projects.json` should include top ticket state when present

## Non-goals

- do not add comments, reactions, or assignee lookup tables
- do not add notification sending
- do not add GitHub Webhooks in this pass
