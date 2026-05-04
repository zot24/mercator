# ADR 0004 — Tests before code, especially for refactors

**Status:** Accepted (2026-05-04)
**PRs:** [#47](https://github.com/zot24/mercator/pull/47) (the safety-net example), [#48](https://github.com/zot24/mercator/pull/48) (the refactor it protected)

## Decision

For non-trivial work — and **always** for refactors / module extractions — write the test first, then the code. For module extractions specifically, the test goes in **before** the code-movement PR, in its own PR, against the *current* implementation. The extraction PR then has to keep those tests passing without modifying them.

## Why

Two concrete failure modes we wanted to avoid:

1. **Silent behavior drift in refactors.** Module splits feel mechanical — you're "just moving code." But moving code touches visibility, imports, lifetimes, and trait coherence in ways that are easy to get subtly wrong. Without a regression net you're shipping on hope. The lesson came from real production failures (not on this project) where a "no-op refactor" landed a one-line semantic change that took weeks to surface.

2. **Retroactive tests rubber-stamp the implementation.** If you write the test after the code, you tend to test what you just wrote — which means the tests pass because the test author knows what the implementation does, not because the implementation is correct. Tests written before the code (or before a refactor) are written against an *intent*, not a known answer.

## How we apply it on this project

- **Pure functions, parsers, formatters, classifiers** — easy TDD wins. The existing `tests` module in `src/main.rs` started this way: `sanitize_filename_*`, `strip_inline_md_*`, `yaml_escape_*`, `render_project_markdown_*` were all written test-first.
- **FS-touching code** — use `tempfile` (added as a dev-dep in [#47](https://github.com/zot24/mercator/pull/47)). Constructed fixture, called the function, asserted on the return + side effects. `survey_projects`, `scan_obsidian_vault`, `detect_tech_stack`, `detect_agent` are all covered this way.
- **Network-touching code** — most fetchers can't be tested in CI without a mock server, which adds dependency surface. We accept that GitHub/GitLab fetchers are integration-tested by smoke runs against the real API, not unit-tested. The `format_api_error` helper *is* unit-tested because it's pure.
- **Refactors** — the canonical pattern played out across [#47](https://github.com/zot24/mercator/pull/47) → [#48](https://github.com/zot24/mercator/pull/48):
  1. PR-A adds 22 characterization tests for the functions that are *about to move*. Tests are in `main.rs::tests` because that's where the implementations are. Green on master.
  2. PR-B moves the implementations to `src/sources.rs`. Tests stay in `main.rs::tests`, just gain a `use crate::sources::*;` import. **Zero test logic changes.** That's the proof the refactor is behavior-preserving.

## Two real bugs the discipline caught

### #1 — Stale-JSON re-import (caught at smoke test → promoted to regression test)

In [#56](https://github.com/zot24/mercator/pull/56) the smoke test exposed that `db::import_from_json` was upserting projects unconditionally before honoring the sidecar blocklist. A user purges a project from the dashboard → DB drops the row, blocklist gains it; but the next survey reads the still-stale `map.json` and silently re-introduces the purged project.

The bug existed for two stages and only surfaced when stage 3b's clean cutover put it in a position where a user would actually run into it. The fix shipped as part of the same PR with a regression test (`import_skips_paths_on_the_purge_blocklist`) that pins the behavior.

### #2 — FTS5 hyphen parser (caught running against real data)

In [#58](https://github.com/zot24/mercator/pull/58) the unit tests all used names like `alpha`, `beta`, which never tripped FTS5's parser. The first run against `~/Desktop/code` hit `mercator search 'cli-tool'` and returned `Error: no such column: tool` — FTS5 reads `-` as the NOT operator. See [ADR 0002](0002-fts5-default-mode-and-token-quoting.md) for the parser fix.

That bug couldn't have been caught by `tempfile`-based unit tests with synthetic project names. The lesson: **smoke-test against real data, not just synthetic fixtures**, especially for anything that takes user input.

## Consequences

**What we gain:**

- Refactor PRs become easy to review: "the tests didn't change, the implementations moved."
- Bugs surface inside the PR that introduces them, not weeks later.
- The test suite grows with the project naturally — every change adds coverage of *new* surface, not old.

**What we pay:**

- Slightly more PR overhead for refactors (two PRs instead of one). Trade-off is reviewability + safety vs. ceremony. We've taken the safety side every time and never regretted it.
- Network-bound code stays under-tested by design. We compensate with smoke tests during PRs that touch fetchers.

## How this rule got established

Mid-session in the multi-PR refactor work that landed #11's wave 2 ([#48](https://github.com/zot24/mercator/pull/48)), the user typed: *"remember tdd and shit first"*. That sentence is the ADR. Everything after it followed the pattern above; everything before it didn't (and it shows in the test count — the project went from 51 tests to 97 in a single session that prioritized this discipline).

This ADR exists because the rule is too easy to forget under pressure. If you're working on Mercator and a refactor feels too small to test first, you're probably wrong; write the test anyway.
