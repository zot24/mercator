# ADR 0003 — `Source` plug-point uses enum dispatch, not `Box<dyn Source>`

**Status:** Accepted (2026-05-04)
**Issue:** [#9](https://github.com/zot24/mercator/issues/9) — closed
**PR:** [#50](https://github.com/zot24/mercator/pull/50)

## Decision

The `Source` trait in `src/sources.rs` is implemented by per-provider structs (`GitHubSource`, `GitLabSource`), and the survey loop holds them as variants of an `AnySource` enum:

```rust
pub trait Source {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn fetch(&self) -> impl Future<Output = Result<Vec<Project>, SourceError>> + Send;
}

pub enum AnySource {
    GitHub(GitHubSource),
    GitLab(GitLabSource),
}
```

The survey loop iterates `Vec<AnySource>` and calls `.fetch().await` per element. The issue's spec called for `Vec<Box<dyn Source>>` and `futures::join_all`. We did neither.

## Why

Two coupled concerns drove this.

### Concurrency is a separable change

The issue's "concurrent fetch via `futures::join_all`" requirement is a behavior change with its own trade-offs:

- Adds a `futures` (or `tokio::try_join_all`) dep.
- Changes the order of `eprintln!` log lines users see during a survey — they're currently sequential and predictable; concurrent fetches interleave them.
- Changes error timing — sequential fetch fails fast on the first source error and skips the rest; concurrent fetch lets all sources fail independently.

None of those are bad, but they're *different* from "introduce a plug-point so #8 deploy integrations don't have to copy-paste the GitHub fetcher." The plug-point is the prerequisite; the concurrency is the optimization. We split them.

### `Box<dyn Source>` requires another dep

Native `async fn` in traits stabilized in Rust 1.75 but isn't dyn-compatible without help. To get `Box<dyn Source>` we'd have to add the `async-trait` macro crate, which boxes every async fn return type. That's two new deps (`async-trait` + `futures`) for a refactor whose user-visible change is **zero**.

`AnySource` enum dispatch gives us the same plug-point for the survey loop with zero new deps. New providers (Vercel, Supabase, Turso for [#8](https://github.com/zot24/mercator/issues/8)) add a struct + an `impl Source` + an `AnySource` variant. The survey loop's match expands by one arm.

## Alternatives considered

| Approach | Deps added | Survey-loop concurrency | Survey-loop ergonomics |
|---|---|---|---|
| `Vec<Box<dyn Source>>` + `async-trait` + `futures::join_all` | `async-trait`, `futures` | Yes | Slightly cleaner — no enum match |
| `Vec<Box<dyn Source>>` + native async-trait + Pin/Box hand-rolled | None | Yes, but with verbose return types | Awkward |
| **`Vec<AnySource>` enum dispatch** (chosen) | None | No (sequential) — separable later | One match per call site |
| Free async fns, no trait, just a sequential chain of `if let Some(...)` | None | No | What we had before #9 — copy-paste per provider |

## Consequences

**What we gain:**

- Plug-point ready for [#8](https://github.com/zot24/mercator/issues/8) (Vercel / Supabase / Turso). New provider = one struct + one `impl Source` + one `AnySource` variant. The existing fetch loop in `mercator survey` doesn't care.
- Zero new deps. Both `async-trait` and `futures` would have been small dependencies, but "small" still has a maintenance cost; not adding either is the cheaper default.
- Per-source `description()` lets each provider own its log line — GitHub still mentions the unauth 60/hr cap; GitLab doesn't (because GitLab doesn't have one).

**What we pay:**

- New providers can't be loaded as plugins from a config file or CLI flag — adding one is a code change. For a single-user tool with three known providers in flight, this is fine. If Mercator ever grows a plugin loader, the `Box<dyn Source>` path is still open; this ADR doesn't close it.
- The survey loop is sequential, so adding GitHub + GitLab fetches doesn't run them in parallel. On the corpora we've measured (~50 GitHub repos), the wire latency dominates and concurrency would help. Captured as follow-up; not blocking #8.

## Reasoning we want to remember

The issue's acceptance criteria conflated two changes ("plug-point" and "concurrency") into one bullet. The implementation cost wasn't the same as the design cost — concurrency adds two deps, has user-visible behavioral implications (log order, error timing), and isn't the thing #8 actually unblocks. Splitting them in the PR boundary kept review simple and let us ship the plug-point cleanly. The lesson: when an issue's acceptance criteria bundle multiple concerns, stage them.
