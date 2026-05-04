# Mercator — Goals

> Vision-level direction. For the live state — what's actually shipped and what's currently in flight — see **[docs/STATUS.md](docs/STATUS.md)**.

## Phase 1 — Make it real (✅ shipped 2026-05-04)

- [x] One screen that shows every project I have (local, GitHub, GitLab, Obsidian)
- [x] Fast search and filter across all of it (SQLite + FTS5) — closed by [#24](https://github.com/zot24/mercator/issues/24); see [ADR 0001](docs/decisions/0001-sqlite-staged-migration.md), [ADR 0002](docs/decisions/0002-fts5-default-mode-and-token-quoting.md)
- [x] Per-project deep-dive page so I stop re-doing archaeology on context switches — file-tree explorer, smart auto-open, dirty/stale badges
- [x] CLI access (`list`, `search`, `export`) so I'm not locked to the UI — closed by [#25](https://github.com/zot24/mercator/issues/25)
- [x] Markdown export — one file per project — that any other tool can consume
- [x] Provider trait so adding new sources (Vercel, Supabase, Turso) is plug-in work, not surgery — closed by [#9](https://github.com/zot24/mercator/issues/9); see [ADR 0003](docs/decisions/0003-source-trait-enum-dispatch.md)
- [ ] Dogfood for a week and decide what hurts most ← this is the open Phase-1 item now

## Phase 2 — Make it smart

- [ ] LLM synthesis per project ("where I left off" stops being a stub)
- [ ] Deployment status (Vercel, Supabase, Turso) on the same screen as the code — [#8](https://github.com/zot24/mercator/issues/8); the `Source` trait from #9 is the plug-point this uses
- [ ] Smarter graph — semantic edges, not keyword overlap — [#27](https://github.com/zot24/mercator/issues/27)
- [ ] Mercator export feeds my Obsidian LLM-wiki, maintained by Claude Code — [#22](https://github.com/zot24/mercator/issues/22)
- [ ] Cost visibility — at minimum dev-time AI spend per project (already in swarm logs) — [#28](https://github.com/zot24/mercator/issues/28)

## Phase 3 — Close the loop

- [ ] Swarm workflow templates (bootstrap MVP, weekly maintenance, idea→implementation) — [#29](https://github.com/zot24/mercator/issues/29)
- [ ] Trigger workflows directly from Mercator with per-project guardrails — [#30](https://github.com/zot24/mercator/issues/30)
- [ ] Mercator + wiki + swarm running as one personal AI dev platform — for me first, then everyone else — the umbrella ([#20](https://github.com/zot24/mercator/issues/20) is the cross-project AI piece)

## Non-goals (on purpose)

- Not a team tool. Single-user until proven otherwise.
- Not a generic "any stack" tool. Opinionated on the indie-AI-SaaS stack I actually use.
- Not a replacement for GitHub/Obsidian/Vercel. A layer above them.
- Not a feature factory. If it doesn't change a decision I make, it doesn't ship.
