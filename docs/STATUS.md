# Mercator — Current State

**Last updated:** 2026-09-19
**Latest tag:** v0.7.5 (`Cargo.toml` `version = "0.7.5"`, released 2026-06-21). `master` is ahead of that tag: the enrichment plug-point (#89) and the serve import guard (#91) are unreleased.
**Test count:** 174 unit tests, all gated by CI

This is the *living state* doc. [GOALS.md](../GOALS.md) is the long-term direction; [CLAUDE.md](../CLAUDE.md) is the operator's manual; this is "where are we right now." If you're picking up the project after time away, read this first.

---

## What just shipped

**2026-09-19 — the DB stops lying, and the docs match the code** ([#90](https://github.com/zot24/mercator/issues/90) via [#91](https://github.com/zot24/mercator/pull/91); [#92](https://github.com/zot24/mercator/issues/92)).

- `mercator serve` used to re-import `mercator_map.json` into the DB on every start, re-stamping `last_seen` on every row and reverting any DB-side change the snapshot did not carry (`POST /api/survey/refresh` and `POST /api/categorize` write the DB, never the JSON). The import now runs only when the `projects` table is empty — first-run hydration for a JSON-only install — via `db::import_from_json_if_empty`. `survey` keeps its own import and its JSON write.
- `/api/skills` built its per-project usage list from the JSON map; it now reads `db::load_all_projects`. `compute_skill_groups` takes `&[Project]`.
- **Phase 3 is retired.** The owner runs a separate ops loop that dispatches agents, proves results, and records decisions; Mercator stays the map. #20, #28, #29 and #30 are closed. See [GOALS.md](../GOALS.md).
- The three prose docs were swept against the binary and the source: every command in `--help` has a README section, every route in `main.rs` is in the table below, version and schema strings are current.

The session ending 2026-07-08 landed the **enrichment plug-point** and the first slice of **#8** — cross-project visibility of GitHub issues and Vercel deploy status.

- **New extension axis.** `enrichment.rs` adds an `Enrichment` trait + `AnyEnrichment` enum dispatch (mirrors `Source`/`AnySource`, ADR 0003) for *per-project child data* that can't ride `Source` (whose `fetch → Vec<Project>`). `EnrichmentError` finally carries the structured `Network`/`Api`/`Parse` split #8 called for (in the new module, not the legacy `SourceError`). **schema v6** adds `github_issues`, `github_remotes`, `vercel_deployments`, and `enrichment_state`, all keyed by `remote_url`.
- **GitHub issues → kanban.** `github.rs` ingests issues (paginated reqwest, delete-then-insert) and the dashboard gains a read-only **KANBAN** view (`/api/issues`) with lanes derived from issue state + a `status:` label convention.
- **Vercel deploy status → badges.** `vercel.rs` pulls the latest production deployment per project from `GET /v9/projects`, joins to surveyed projects by normalized git remote, and the dashboard badges each project row/card (`/api/deployments`) green/red/amber by state.
- **Wiring.** Enrichment runs after project upsert at both dispatch sites (`survey`, `POST /api/survey/refresh`) and on-demand via the new `mercator enrich` command. Each provider opens its own short-lived write connection (WAL + `busy_timeout`) so a network fetch never holds the shared DB lock. Config gains a `[vercel]` block.
- **Still open on #8:** Supabase/Turso providers, kanban write-back, and aligning `SourceError` to the `EnrichmentError` taxonomy.

## Previously shipped

The session ending 2026-06-19 made `mercator list` usable as a "what needs attention" view. Two parts:

- **Pretty output.** `--format text` is now TTY-aware: an aligned, lightly-coloured table (`TYPE / NAME / SYNC / TECH / PATH`, home dir abbreviated to `~`, `NO_COLOR` respected) when stdout is a terminal, and the unchanged tab-separated rows when piped — so `awk` / `cut` / `grep` pipelines keep working untouched. `search` shares the same renderer.
- **Attention filters.** `--no-git` (Folder/Idea — not a repo), `--no-remote` (no `origin` configured — covers folders *and* remote-less repos), and `--out-of-sync` (branch ahead and/or behind its upstream). The last is backed by new per-repo divergence counts: `git_ahead` / `git_behind` on `projects` (**schema v4**, added via `ALTER TABLE`), populated at survey time from `git rev-list --left-right --count @{u}...HEAD`. **No `git fetch`** runs — counts reflect the cached upstream as of the last fetch, keeping survey offline and fast. Both columns are NULL for non-git projects and repos with no remote-tracking branch, so `--out-of-sync` keys off `> 0`. The counts also ride along in `--format json` / `/api/map` as `gitAhead` / `gitBehind`.

The session ending 2026-05-27 added a first-class **active set** — a path the user is currently working on, stored in `active_projects` (schema v3), managed via `mercator active add/remove/list/export`, and auto-exported to `active-projects.json` for Hermes (or any session-loader) to consume on each session start. The active set is *orthogonal* to surveyed state: it survives re-surveys, and a path can be activated before it's surveyed (and remains in the snapshot, just without enrichment). `mercator list --active` filters the existing project list to active projects only.

The session ending 2026-05-04 closed three large issues:

| Issue | Title | Status |
|---|---|---|
| [#9](https://github.com/zot24/mercator/issues/9) | Provider trait | Closed — see [ADR 0003](decisions/0003-source-trait-enum-dispatch.md) |
| [#24](https://github.com/zot24/mercator/issues/24) | SQLite + FTS5 migration | Closed — see [ADR 0001](decisions/0001-sqlite-staged-migration.md) |
| [#25](https://github.com/zot24/mercator/issues/25) | `mercator list` / `search` CLI | Closed (closed by the same PR that finished #24's stage 4b) |

Plus extracted six modules out of `main.rs` (closing [#11](https://github.com/zot24/mercator/issues/11)): `src/project.rs`, `src/markdown.rs`, `src/tags_graph.rs`, `src/skills.rs`, `src/sources.rs`, `src/agent.rs`, plus `src/db.rs` and `src/config.rs` born already-modular. `main.rs` went 3604 → ~2.1k lines and held only CLI parsing + HTTP handlers + AppState wiring; the enrichment, settings, ticket and readme handlers have since grown it back to ~3.6k lines (3582 as of 2026-09-19).

Test count went 0 → 102, closing [#12](https://github.com/zot24/mercator/issues/12). See [ADR 0004](decisions/0004-tdd-discipline.md) for the discipline rule.

---

## Where the data lives now

**Primary store: `mercator.db`** (SQLite, schema v6). Created and migrated on first `mercator survey` or `mercator serve`. PRAGMA `journal_mode = WAL` for read/write concurrency. Foreign keys on; cascades clean up the M2M relations.

```
mercator.db
├── projects            -- one row per surveyed project (Git/Folder/Idea/GitHub/GitLab/Obsidian); git_ahead/git_behind added in v4
├── tags                -- normalized; populated by auto_tag_projects
├── project_tags        -- M2M
├── tech_stack          -- normalized; populated by detect_tech_stack
├── project_tech        -- M2M
├── obsidian_links      -- 1:1, optional, projects↔Obsidian URI
├── purged              -- blocklist; survives surveys
├── active_projects     -- "currently working on" set; survives surveys; path-keyed (schema v3)
├── local_tickets       -- `POST /api/tickets` with source=local (v5); written, not yet read back by anything
├── github_remotes      -- owner/repo per canonical remote + last refresh (v6, GitHub-issues enrichment)
├── github_issues       -- one row per issue/PR keyed by (remote_url, issue_number) (v6)
├── vercel_deployments  -- latest deployment per project, keyed by uid, joined to projects by remote_url (v6)
├── enrichment_state    -- one row per (provider, scope): last run, ok flag, last error (v6); written, no reader yet
└── projects_fts        -- FTS5 virtual table over name + description + tags
```

`active_projects` is path-keyed with no FK to `projects` for the same reason `purged` is: re-survey runs replace the project rows but must not blow away orthogonal state. A path can also be activated before it's surveyed — `list_active` returns the raw set, `list_projects` with `active=true` filters to only the rows that exist in both tables.

**Legacy store: `mercator_map.json`** (still written by `mercator survey` as a backup snapshot). `mercator serve` imports it **only when the DB has no projects** (the first-run upgrade path, #91); after that it is read only as a fallback when a DB read errors. `mercator_purged.json` is **no longer written** — the `purged` table superseded it — and is read only during that same first-run import.

**Migration path:** existing installs running v0.1.x with only the JSON files get auto-imported on first run of the new binary. The import is idempotent and respects the blocklist (a regression test pins this — see [ADR 0001](decisions/0001-sqlite-staged-migration.md) and [ADR 0004](decisions/0004-tdd-discipline.md) for the bug story).

---

## What every surface looks like today

### CLI

| Command | Reads | Writes |
|---|---|---|
| `mercator survey <paths...> -d <db> -o <map.json> [--github U] [--gitlab U] [--obsidian V] [-w MIN]` | filesystem, GitHub/GitLab APIs, Obsidian vault, DB blocklist, config.toml | DB upsert + JSON snapshot, then enrichment tables when a token is configured |
| `mercator serve -m <map> -d <db> [--refresh P]` | DB; the JSON only on an empty DB (first-run import) or as a fallback on a DB read error | DB via handlers; one-time first-run import |
| `mercator enrich [--github-token T] [--owned-only] -d <db>` | DB projects, config.toml, GitHub + Vercel APIs | `github_issues`, `github_remotes`, `vercel_deployments`, `enrichment_state` |
| `mercator export <out> -d <db> [--obsidian-vault V]` | DB | Markdown files |
| `mercator list -d <db> [--type T] [--tag T] [--tech T] [--active] [--no-git] [--no-remote] [--out-of-sync] [--format text\|json]` | DB | stdout (table on a TTY, tab-separated when piped, or JSON) |
| `mercator search <query> -d <db> [--format text\|json]` | DB FTS5 | stdout (tab-separated or JSON) |
| `mercator readme [--inject FILE] [--all] [--list] [--public-only] …` | DB; GitHub API / web for `--public-only` | stdout, or the block between `<!-- MERCATOR:START/END -->` in `FILE` |
| `mercator active add <path> [--note ..]` | DB | DB + `active-projects.json` |
| `mercator active remove <path>` | DB | DB + `active-projects.json` |
| `mercator active list [--format text\|json]` | DB | stdout (tab-separated or JSON) |
| `mercator active export` | DB | `active-projects.json` |

`-d/--db` defaults to `mercator.db` for every command. `-o/--output` (survey) and `-m/--map-file` (serve) default to `mercator_map.json` for the migration / fallback path; you can pass `-o /dev/null` if you don't want the JSON snapshot.

### HTTP API

Every `/api/*` endpoint reads and writes the DB, including `/api/skills` (which then walks the filesystem for `.claude/skills`). The JSON fallback only triggers if a DB read errors. Token auth via `MERCATOR_TOKEN` guards every route; the static dashboard is unauthenticated.

| Endpoint | Method | Reads | Writes |
|---|---|---|---|
| `/api/map` | GET | DB | — |
| `/api/graph` | GET | DB | — |
| `/api/project/purge` | POST | DB | DB tx (delete project + insert purged) |
| `/api/project/restore` | POST | DB | DB |
| `/api/purged` | GET | DB | — |
| `/api/project/tree` | GET | filesystem | — |
| `/api/project/file` | GET | filesystem | — |
| `/api/git-status` | GET | git CLI | — |
| `/api/categorize` | POST | DB | DB upsert |
| `/api/survey/refresh` | POST | DB blocklist + filesystem | DB upsert |
| `/api/skills` | GET | DB project list + `~/.claude/skills`, plugin cache, each project's `.claude/skills` | — |
| `/api/issues` | GET | DB (`github_issues`, PRs excluded) + lane derivation | — |
| `/api/project/issues` | GET | DB (`github_issues` for one canonical remote) | — |
| `/api/deployments` | GET | DB (`vercel_deployments`) | — |
| `/api/settings` | GET | in-memory config (redacted: users + `*_token_set` + `vercel_team`) | — |
| `/api/settings` | POST | in-memory config | `~/.config/mercator/config.toml` (GitHub/GitLab user + token only; no `[vercel]` fields) |
| `/api/tickets` | POST | config token | `local_tickets` insert, or a GitHub issue via the API |
| `/api/open-terminal` | POST | — | spawns Terminal.app (macOS) |
| `/api/agent/*` (swarm-feature) | various | swarm crate | swarm crate; 404 in default builds |

### Module map

```
src/
├── main.rs           -- CLI parsing, HTTP handlers, AppState, route registration
├── db.rs             -- SQLite schema, migrations, CRUD, FTS5 search/list
├── project.rs        -- `Project` struct + JSON load/save (legacy snapshot path)
├── sources.rs        -- local FS survey, GitHub/GitLab fetchers, Obsidian, dedup, Source trait
├── enrichment.rs     -- Enrichment trait + AnyEnrichment dispatch + run_all (#8)
├── github.rs         -- GitHub-issues enrichment provider + kanban lane mapping
├── vercel.rs         -- Vercel deploy-status enrichment provider
├── config.rs         -- ~/.config/mercator/config.toml (GitHub / GitLab / Vercel)
├── markdown.rs       -- description extraction, frontmatter, export rendering
├── tags_graph.rs     -- auto-tagging + D3 graph computation
├── skills.rs         -- skills inventory walker
├── readme.rs         -- `mercator readme` rendering + marker splice
├── ticket.rs         -- `POST /api/tickets` (local_tickets insert / GitHub issue create)
└── agent.rs          -- swarm-feature agent runner (cfg-gated)
```

---

## Where we're heading

In rough priority order; the project board is authoritative, this is the human-readable summary.

### Next, smaller

1. **Dogfood a fresh survey.** Nothing has exercised the map since 2026-06-21, and that survey's paths no longer exist on disk. Build from `master` (the Homebrew release is v0.7.5, without enrichment), run `mercator survey ~/code` into a new DB, use it for a few evenings, then add a dated heading here that names one next move and says whether survey-root config ([#26](https://github.com/zot24/mercator/issues/26)) is why the old map went stale. No new feature before that heading exists.

### Recently shipped

- ✅ **Honest docs sweep** — closes [#92](https://github.com/zot24/mercator/issues/92). README, this file and CLAUDE.md are diffed against `--help`, the route list and `src/db.rs`; Phase 3 retired.
- ✅ **Serve import guard + skills from the DB** — closes [#90](https://github.com/zot24/mercator/issues/90).
- ✅ **Settings panel UI cutover** — closes [#2](https://github.com/zot24/mercator/issues/2). The dashboard reads `GET /api/settings` for the redacted shape and writes via `POST /api/settings` (with `clear_token` for explicit removal). Legacy `mercator-settings` localStorage rows are migrated on first settings open. The token field never re-populates from server — placeholder shows "(saved — leave blank to keep)" so an unchanged blank input doesn't accidentally erase a stored token.
- ✅ **Server-side config for tokens** — `~/.config/mercator/config.toml` (mode 0600), refresh button now fetches GitHub/GitLab when the config has a user.
- ✅ **Concurrent fetch in survey** — `futures::future::join_all` over the `Source` trait. Logs stay deterministic (intent lines before the await, result lines after). See [ADR 0003](decisions/0003-source-trait-enum-dispatch.md) "Update (2026-05-04)".

### Phase 2 — Make it smart

Big tickets:

| Issue | Title |
|---|---|
| [#8](https://github.com/zot24/mercator/issues/8) | Deploy-target integrations (Vercel, Supabase, Turso) — via the enrichment plug-point, not the `Source` trait; Vercel shipped in #89 |
| [#22](https://github.com/zot24/mercator/issues/22) | LLM-wiki layer — synthesize project notes back into Obsidian |
| [#27](https://github.com/zot24/mercator/issues/27) | Replace keyword-overlap graph with semantic embeddings |

#8's Vercel part shipped in #89; Supabase / Turso remain. #22 is the Mercator→wiki join and stays open. #27 is deferred until the fresh survey says the graph is where decisions get made.

### Phase 3 — retired (2026-09-19)

Workflow templates, per-project guardrails, triggering agents from the dashboard, and cross-project AI questioning are not Mercator work. The owner's ops loop already dispatches agents, proves results, and records decisions; building a second loop inside the map would duplicate it. #20, #28, #29 and #30 are closed. The opt-in `swarm` agent runner stays as it is (compiled out of default builds) and will not be distributed.

### Cross-cutting

- [#21](https://github.com/zot24/mercator/issues/21) — make `swarm` distributable. Retired with Phase 3: the runner stays a local opt-in (`--features swarm` plus a hand-added path-dep); the remaining gap is that the dashboard draws RUN / LAUNCH / AGENTS in default builds where those routes 404
- [#26](https://github.com/zot24/mercator/issues/26) — auto-discover sources from config — Obsidian + future deploy targets (the GitHub/GitLab piece shipped via the `~/.config/mercator/config.toml` work; Obsidian + deploy targets remain)

---

## Known sharp edges

1. **`cargo build --features swarm` fails on a clean clone.** The `swarm` path-dep is intentionally not on the manifest. Add `swarm = { path = "../swarm" }` to `[dependencies]` first. ([#21](https://github.com/zot24/mercator/issues/21).)
2. **GitHub / GitLab fetches surface errors via `eprintln!`** but don't propagate them as structured `SourceError` variants yet — the variant exists (`SourceError::Generic(String)`). The structured `Network`/`Api`/`Parse` split landed in `EnrichmentError` (#8); aligning `SourceError` to it is a follow-up.
3. **Two settings-panel fields are dead UI.** LOCAL PATHS and GITLAB URL write only to browser `localStorage` and drive nothing: refresh paths come from `serve --refresh`, and the GitLab fetcher is fixed to gitlab.com. The token fields are live ([#2](https://github.com/zot24/mercator/issues/2) closed them). The `[vercel]` block cannot be set from the dashboard — `POST /api/settings` accepts GitHub/GitLab fields only — so edit `~/.config/mercator/config.toml` for it.
4. **`osascript` Terminal launcher is macOS-only.** Closed as wontfix-for-now ([#17](https://github.com/zot24/mercator/issues/17)).
5. **Docker default is loopback-bind**; expose with `-b 0.0.0.0` *and* `MERCATOR_TOKEN`. ([#6](https://github.com/zot24/mercator/issues/6) shipped the auth piece; the bind default stays loopback for safety.)
6. **`agent_jobs` table is not in the schema.** The swarm-feature agent runner keeps job state in process memory; restarting the binary loses the job list. With Phase 3 retired this is not scheduled.
7. **The dashboard shows RUN / LAUNCH / AGENTS in every build.** Those controls call `/api/agent/*`, which exists only with `--features swarm`; in a default build they answer 404 and the AGENTS count stays at 0.
8. **`enrichment_state` and `local_tickets` are written and never read.** No route or view surfaces provider sync health or local tickets yet ([docs/TICKET_CONTRACT.md](TICKET_CONTRACT.md) describes the intended consumers).

---

## How to verify any of this

```bash
# All tests
cargo test

# CI's three gates
cargo fmt --all --check
cargo clippy --all-targets --no-deps -- -D warnings
cargo build && cargo test

# Smoke-test the live binary against your real corpus
mkdir -p /tmp/mercator-live
cargo run --release -- survey ~/Desktop/code -d /tmp/mercator-live/test.db
cargo run --release -- list -d /tmp/mercator-live/test.db --type Git | head
cargo run --release -- search 'whatever' -d /tmp/mercator-live/test.db
cargo run --release -- serve -d /tmp/mercator-live/test.db
```

## Where things are documented

- **Long-term direction:** [GOALS.md](../GOALS.md)
- **Operator's manual** (how to work with the codebase): [CLAUDE.md](../CLAUDE.md)
- **Public-facing pitch:** [README.md](../README.md)
- **Major design decisions:** [docs/decisions/](decisions/)
- **Ticket creation contract:** [docs/TICKET_CONTRACT.md](TICKET_CONTRACT.md) (`POST /api/tickets`; creation is implemented, the consumers are not)
- **This file:** current state and where we're heading

If a decision lives only in a commit message or PR description, that's a sign it should probably be promoted to an ADR.
