# ADR 0001 — SQLite + FTS5 migrated in 7 staged PRs, not one

**Status:** Accepted (2026-05-04)
**Issue:** [#24](https://github.com/zot24/mercator/issues/24) — closed
**PRs:** [#51](https://github.com/zot24/mercator/pull/51), [#52](https://github.com/zot24/mercator/pull/52), [#53](https://github.com/zot24/mercator/pull/53), [#54](https://github.com/zot24/mercator/pull/54), [#55](https://github.com/zot24/mercator/pull/55), [#56](https://github.com/zot24/mercator/pull/56), [#57](https://github.com/zot24/mercator/pull/57), [#58](https://github.com/zot24/mercator/pull/58)

## Decision

Migrate `mercator_map.json` → SQLite + FTS5 across **eight PRs in seven stages**, with each stage shippable on its own and each one keeping the JSON path live as a fallback until the cutover is verified.

The stages, in order:

| Stage | Cut |
|---|---|
| 1 | Add `src/db.rs` + schema + JSON-import migration. **No reads from DB anywhere.** Dual-write so the user can verify the DB matches the JSON they already trust. |
| 2a | Switch `/api/map` and `/api/graph` to read from SQLite. JSON fallback on DB error. Survey continues to dual-write. |
| 2b | Purge / restore / list go through SQLite. Atomic transactions for purge. JSON sidecar still mirrored. |
| 2c | Refresh + recategorize go through SQLite. With this every API endpoint is DB-backed. |
| 3a | Drop the JSON mirror writes from dashboard handlers. SQLite is the sole writer. JSON snapshot still readable as a fallback. |
| 3b | Survey CLI + Export read from SQLite. With this no read path in the binary touches `mercator_map.json` for live state. |
| 4a | Add the `projects_fts` virtual table + `db::search_projects` / `db::list_projects` helpers. Schema v2 with v1→v2 migration. No CLI yet. |
| 4b | `mercator list` and `mercator search` CLI subcommands. (Closes [#25](https://github.com/zot24/mercator/issues/25) too.) |

## Alternatives considered

1. **One big PR.** Add the schema, cut every read+write path over, kill the JSON files, ship. Reviewable in a single sitting if the diff is clean.
2. **Two PRs — schema then cutover.** Get the schema in, then a "switch everything" follow-up.
3. **The eight-stage path we took.**

## Why staged

The acceptance criteria on #24 alone ran ~7 schema tables, an FTS5 virtual table, an `agent_jobs` survival path, a JSON migration importer, plus full handler-cutover. Doing all of that in one PR meant:

- **Unreviewable diff.** ~1500 lines touching every read and write path. Reviewer fatigue → bugs miss the gate.
- **No incremental verification.** If something broke at minute 30 of running the new binary, we'd have to roll back the entire migration to debug. Staged means we can roll back the stage that broke and keep the rest.
- **No room for the bug we did catch.** Stage 3b's smoke test exposed a real correctness bug — `db::import_from_json` was upserting projects unconditionally, so a stale `map.json` (still containing a path the user had purged via the dashboard) silently re-introduced the purged project on the next survey. That bug only became visible *because* stage 3a had isolated the write paths and stage 3b had a clean enough surface to smoke-test in isolation. In a single mega-PR it would have shipped.

## Consequences

**What we gain:**

- Reviewable atomic units — every PR fits one mental session.
- Each stage was end-to-end smoke-tested against the live binary before merge. Bugs surfaced inside the stage that introduced them, not in production weeks later.
- The user could pull master at any stage boundary and have a working binary.
- The "JSON fallback on DB read failure" pattern from stage 2a still survives — a corrupted DB doesn't blank the dashboard, it falls through to the last-known JSON snapshot.

**What we paid:**

- Eight commits and eight CI runs instead of one.
- Each stage had to be carefully scoped to *not* depend on later stages, which sometimes meant carrying a temporary helper or `#[allow(dead_code)]` annotation.
- The stages-3a/3b split was particularly fiddly — 3a kept the survey CLI on JSON for blocklist reads while 3b moved it to DB. We had to think about what the binary would do if a user upgraded mid-stage.

## What's still left

- **`agent_jobs` table.** The issue spec mentioned it; we deferred. Today the swarm-feature agent runner keeps its job state in process memory (`Arc<Mutex<Vec<AgentJob>>>`), which loses everything on restart. A future stage adds a table + reads it on startup. Not urgent because the swarm path-dep isn't on the manifest by default.
- **Concurrent fetch in survey.** [ADR 0003](0003-source-trait-enum-dispatch.md) explains why we ship sequential fetch; concurrency is a separable change.

## Reasoning chain we want to remember

Before starting #24 the queue was "do the SQLite migration." The migration *as written in the issue* is a single story. The shippable atom is much smaller. Most large refactors fail because they're written as one story and shipped as one PR; the lesson here is that the issue's description shape and the ship shape don't have to be the same.
