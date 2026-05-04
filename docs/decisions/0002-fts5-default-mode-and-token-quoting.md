# ADR 0002 — Default-mode FTS5 + per-token phrase quoting

**Status:** Accepted (2026-05-04)
**Issue:** [#24](https://github.com/zot24/mercator/issues/24)
**PRs:** [#57](https://github.com/zot24/mercator/pull/57) (FTS5 plumbing), [#58](https://github.com/zot24/mercator/pull/58) (token-quoting parser fix)

## Decision

Two coupled choices about how `mercator search` works:

1. The `projects_fts` virtual table uses **default-mode FTS5** (no `content=` clause), not contentless or external-content.
2. The `mercator search` CLI **wraps each whitespace-separated token in double-quotes** before passing it to `MATCH`, so punctuation inside a word is literal and multi-word queries become AND across tokens. Users lose direct access to FTS5's prefix / OR / column-filter syntax in exchange.

## Alternatives considered for #1 (FTS5 mode)

FTS5 has three storage modes. We tried two of them.

| Mode | Storage | DELETE FROM works? | What we'd have to do |
|---|---|---|---|
| **Default** (chosen) | FTS5 stores its own copy of indexed columns | Yes | DELETE+INSERT per project upsert. Slightly redundant with `projects` table. |
| **Contentless** (`content=''`) | FTS5 stores indexes only; data lives elsewhere | No — needs `INSERT INTO ft(ft, rowid, c1, ...) VALUES('delete', ...)` with old values | We tried this first. The DELETE syntax requires knowing the *old* column values, which `upsert_project` doesn't have at hand. Could query them, but adds a SELECT before every upsert. |
| **External content** (`content='projects'`) | FTS5 references projects.id directly | Yes, with care | Requires every indexed column to exist in the projects table. `tags` lives in a separate `project_tags` M2M, so we'd need a denormalized `tags_text` column on projects. More schema surface, more sync points. |

## Why default mode

Per-row sync from `upsert_project` is the hot path — it runs on every survey iteration, every dashboard refresh, every agent re-tag. The contentless and external-content modes both make that path more complex (extra SELECT, or a denormalized column with its own sync problem), and we don't get enough back to justify it.

The redundancy cost is small: the FTS table stores `name`, `description`, and a space-joined `tags` string. The `projects` table stores `name` and `description` for human-friendly reads. The duplication is bounded and well-understood.

If the redundancy ever becomes a problem (it won't), the path forward is external content with a `tags_text` denormalized column and SQLite triggers. Captured here so the future-us doesn't re-litigate.

## Alternatives considered for #2 (query parser)

The first cut passed user input directly to `MATCH`:

```rust
SELECT ... WHERE projects_fts MATCH ?
```

— with the user's query as the `?` bind. This exposes the full FTS5 query syntax: `foo*` for prefix, `foo OR bar`, `name:foo` column filter, etc.

The reality of using it on real project names killed it instantly. The very first smoke test:

```
$ mercator search 'cli-tool'
Error: read search rows: no such column: tool
```

FTS5 reads `-` as the NOT operator. `cli-tool` parses as `cli NOT tool`. Project paths and names are full of hyphens, dots, slashes — punctuation that's syntactically meaningful in FTS5 but isn't what users mean when they type a project name.

Alternatives:

1. **Document the gotcha** ("quote your hyphenated names yourself") — terrible UX, every user trips it.
2. **Escape special chars** in the query string before passing to MATCH — there's no clean escape for the `-` operator; the canonical way is to quote.
3. **Wrap each token in phrase quotes** so each word is literal — what we picked.
4. **Switch to a simpler query syntax** (LIKE `%foo%`) — kills FTS5's whole reason to be there.

## Why per-token phrase quoting

`normalize_fts_query("cli-tool")` becomes `"cli-tool"`. `normalize_fts_query("rust web")` becomes `"rust" "web"` — which FTS5 treats as AND across two literal tokens.

This is what 95% of users mean when they type a query. It's the same behavior `git grep` and `grep -F` give you, which is the comparison most users have in their head.

## Consequences

**What users get:**

- `mercator search foo-bar.baz` finds the project `foo-bar.baz` literally.
- `mercator search rust cli` finds projects mentioning both "rust" and "cli" anywhere in the indexed columns.
- Punctuation in input never produces a parser error.

**What users lose from the CLI:**

- **Prefix matching** (`mercator search 'foo*'` no longer means "starts with foo"). Workaround: drop down to `sqlite3 mercator.db "SELECT path FROM projects p JOIN projects_fts f ON p.id = f.rowid WHERE projects_fts MATCH 'foo*'"`.
- **OR queries** (`mercator search 'foo OR bar'` no longer means "either"). Same workaround.
- **Column filters** (`mercator search 'name:foo'` matches the literal token `name:foo`, not "name column matches foo").

A future flag — `--raw` or similar — could opt back into raw FTS5 syntax. Not added today because nobody's asked. Captured here so the option is on the table.

**What's not in the FTS index at all:**

- `tech_stack` (Rust, Node.js, Docker, …). Use `mercator list --tech Rust` instead. Putting tech_stack in the FTS index would mean searching `mercator search rust` would find every Rust project, which is plausible, but the `--tech` filter already covers it cleanly. Adding tech_stack to FTS is a one-line schema-v3 migration the day it's wanted.

## What surprised us

The FTS5 hyphen bug only surfaced because we ran the binary against `~/Desktop/code` — a real corpus full of hyphenated project names — instead of synthetic test data. The unit tests all used names like `alpha`, `beta`, which would have passed with the original raw-MATCH approach forever. **Smoke-test against real data, not just synthetic fixtures.**
