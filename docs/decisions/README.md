# Architecture Decision Records

This folder captures the *non-obvious* design calls Mercator has made — the ones that aren't visible from the code alone, that you'd otherwise have to mine out of commit messages or PR descriptions.

The format is loose ADR: each file states the **decision**, the **alternatives considered**, the **reason we chose what we chose**, and the **consequences** (including what we lose). When a future change reverses one of these, the ADR doesn't get deleted — it gets a `## Superseded by ...` section pointing forward, so the reasoning chain stays intact.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-sqlite-staged-migration.md) | SQLite + FTS5 migrated in 7 staged PRs, not one | Accepted |
| [0002](0002-fts5-default-mode-and-token-quoting.md) | Default-mode FTS5 + per-token phrase quoting | Accepted |
| [0003](0003-source-trait-enum-dispatch.md) | `Source` plug-point uses enum dispatch, not `Box<dyn>` | Accepted |
| [0004](0004-tdd-discipline.md) | Tests before code, especially for refactors | Accepted |

## When to add an ADR

A non-trivial decision is one where:
- A reasonable person would have picked the other option
- The choice constrains what you can do later
- The reasoning isn't obvious from reading the code six months from now

Build steps, lint configs, and "we used X because the docs said so" don't qualify. Trade-offs do.

## When to amend one

If a decision turns out to be wrong, write a new ADR that **supersedes** the old one rather than editing the old one in place. The old one stays as the historical record so future readers can see the reasoning chain — including the reasoning that turned out to be wrong, which is often the most useful kind.
