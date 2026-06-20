# Mercator — Current State

**Last updated:** 2026-06-19
**Latest tag:** v0.1.x (master)
**Test count:** 138 unit tests, all gated by CI

This is the *living state* doc. [GOALS.md](../GOALS.md) is the long-term direction; [CLAUDE.md](../CLAUDE.md) is the operator's manual; this is "where are we right now." If you're picking up the project after time away, read this first.

---

## What just shipped

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

Plus extracted six modules out of `main.rs` (closing [#11](https://github.com/zot24/mercator/issues/11)): `src/project.rs`, `src/markdown.rs`, `src/tags_graph.rs`, `src/skills.rs`, `src/sources.rs`, `src/agent.rs`, plus `src/db.rs` and `src/config.rs` born already-modular. `main.rs` went 3604 → ~2.1k lines and now holds only CLI parsing + HTTP handlers + AppState wiring.

Test count went 0 → 102, closing [#12](https://github.com/zot24/mercator/issues/12). See [ADR 0004](decisions/0004-tdd-discipline.md) for the discipline rule.

---

## Where the data lives now

**Primary store: `mercator.db`** (SQLite, schema v4). Created and migrated on first `mercator survey` or `mercator serve`. PRAGMA `journal_mode = WAL` for read/write concurrency. Foreign keys on; cascades clean up the M2M relations.

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
└── projects_fts        -- FTS5 virtual table over name + description + tags
```

`active_projects` is path-keyed with no FK to `projects` for the same reason `purged` is: re-survey runs replace the project rows but must not blow away orthogonal state. A path can also be activated before it's surveyed — `list_active` returns the raw set, `list_projects` with `active=true` filters to only the rows that exist in both tables.

**Legacy store: `mercator_map.json`** (still written by `mercator survey` as a backup snapshot, no longer read by the dashboard except as a fallback when the DB read errors). `mercator_purged.json` is **no longer written** — the `purged` table superseded it.

**Migration path:** existing installs running v0.1.x with only the JSON files get auto-imported on first run of the new binary. The import is idempotent and respects the blocklist (a regression test pins this — see [ADR 0001](decisions/0001-sqlite-staged-migration.md) and [ADR 0004](decisions/0004-tdd-discipline.md) for the bug story).

---

## What every surface looks like today

### CLI

| Command | Reads | Writes |
|---|---|---|
| `mercator survey <paths...> -d <db> -o <map.json>` | DB blocklist | DB upsert + JSON snapshot |
| `mercator serve -m <map> -d <db>` | DB; JSON fallback | DB only (handlers) |
| `mercator export <out> -d <db>` | DB | Markdown files |
| `mercator list -d <db> [--type T] [--tag T] [--tech T] [--active]` | DB | stdout (tab-separated) |
| `mercator search <query> -d <db>` | DB FTS5 | stdout (tab-separated) |
| `mercator active add <path> [--note ..]` | DB | DB + `active-projects.json` |
| `mercator active remove <path>` | DB | DB + `active-projects.json` |
| `mercator active list [--format text\|json]` | DB | stdout (tab-separated or JSON) |
| `mercator active export` | DB | `active-projects.json` |

`-d/--db` defaults to `mercator.db` for every command. `-o/--output` (survey) and `-m/--map-file` (serve) default to `mercator_map.json` for the migration / fallback path; you can pass `-o /dev/null` if you don't want the JSON snapshot.

### HTTP API

Every `/api/*` endpoint reads and writes the DB. JSON fallback only triggers if the DB read errors. Token auth via `MERCATOR_TOKEN` works the same as before.

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
| `/api/skills` | GET | filesystem | — |
| `/api/open-terminal` | POST | — | spawns Terminal.app (macOS) |
| `/api/agent/*` (swarm-feature) | various | swarm crate | swarm crate |

### Module map

```
src/
├── main.rs           -- CLI parsing, HTTP handlers, AppState, route registration
├── db.rs             -- SQLite schema, migrations, CRUD, FTS5 search/list
├── project.rs        -- `Project` struct + JSON load/save (legacy snapshot path)
├── sources.rs        -- local FS survey, GitHub/GitLab fetchers, Obsidian, dedup, Source trait
├── markdown.rs       -- description extraction, frontmatter, export rendering
├── tags_graph.rs     -- auto-tagging + D3 graph computation
├── skills.rs         -- skills inventory walker
└── agent.rs          -- swarm-feature agent runner (cfg-gated)
```

---

## Where we're heading

In rough priority order; the project board is authoritative, this is the human-readable summary.

### Next, smaller

1. **Honest README sweep.** README's "Why Mercator?" mentions deploy decay and cross-project AI as if they ship; they don't. Already partially flagged in the Roadmap section — could be tighter.

### Recently shipped

- ✅ **Settings panel UI cutover** — closes [#2](https://github.com/zot24/mercator/issues/2). The dashboard reads `GET /api/settings` for the redacted shape and writes via `POST /api/settings` (with `clear_token` for explicit removal). Legacy `mercator-settings` localStorage rows are migrated on first settings open. The token field never re-populates from server — placeholder shows "(saved — leave blank to keep)" so an unchanged blank input doesn't accidentally erase a stored token.
- ✅ **Server-side config for tokens** — `~/.config/mercator/config.toml` (mode 0600), refresh button now fetches GitHub/GitLab when the config has a user.
- ✅ **Concurrent fetch in survey** — `futures::future::join_all` over the `Source` trait. Logs stay deterministic (intent lines before the await, result lines after). See [ADR 0003](decisions/0003-source-trait-enum-dispatch.md) "Update (2026-05-04)".

### Phase 2 — Make it smart

Big tickets:

| Issue | Title |
|---|---|
| [#8](https://github.com/zot24/mercator/issues/8) | Deploy-target integrations (Vercel, Supabase, Turso) — uses the `Source` trait from #9 |
| [#22](https://github.com/zot24/mercator/issues/22) | LLM-wiki layer — synthesize project notes back into Obsidian |
| [#27](https://github.com/zot24/mercator/issues/27) | Replace keyword-overlap graph with semantic embeddings |
| [#28](https://github.com/zot24/mercator/issues/28) | Per-project cost visibility — aggregate Swarm AI spend |

### Phase 3 — Close the loop

| Issue | Title |
|---|---|
| [#20](https://github.com/zot24/mercator/issues/20) | Cross-project AI (which project could ship this week / which idea has signal) |
| [#29](https://github.com/zot24/mercator/issues/29) | Swarm workflow templates — bootstrap-MVP, weekly-maintenance, idea→implementation |
| [#30](https://github.com/zot24/mercator/issues/30) | Per-project workflow guardrails — branch policy, tool allowlist, cost ceiling |

### Cross-cutting

- [#21](https://github.com/zot24/mercator/issues/21) — make `swarm` distributable so `cargo install` works (today the path-dep has to be added by hand)
- [#26](https://github.com/zot24/mercator/issues/26) — auto-discover sources from config — Obsidian + future deploy targets (the GitHub/GitLab piece shipped via the `~/.config/mercator/config.toml` work; Obsidian + deploy targets remain)

---

## Known sharp edges

1. **`cargo build --features swarm` fails on a clean clone.** The `swarm` path-dep is intentionally not on the manifest. Add `swarm = { path = "../swarm" }` to `[dependencies]` first. ([#21](https://github.com/zot24/mercator/issues/21).)
2. **GitHub / GitLab fetches surface errors via `eprintln!`** but don't propagate them as structured `SourceError` variants yet — the variant exists (`SourceError::Generic(String)`), it just hasn't been split into `Network`/`Api`/`Parse` because no caller needs to discriminate yet. Will happen with [#8](https://github.com/zot24/mercator/issues/8).
3. **Settings panel tokens are dead UI.** ([#2](https://github.com/zot24/mercator/issues/2).)
4. **`osascript` Terminal launcher is macOS-only.** Closed as wontfix-for-now ([#17](https://github.com/zot24/mercator/issues/17)).
5. **Docker default is loopback-bind**; expose with `-b 0.0.0.0` *and* `MERCATOR_TOKEN`. ([#6](https://github.com/zot24/mercator/issues/6) shipped the auth piece; the bind default stays loopback for safety.)
6. **`agent_jobs` table is not in the schema yet.** Today the swarm-feature agent runner keeps job state in process memory; restarting the binary loses the job list. Stage 5 of the SQLite work, not yet scheduled.

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
- **This file:** current state and where we're heading

If a decision lives only in a commit message or PR description, that's a sign it should probably be promoted to an ADR.
