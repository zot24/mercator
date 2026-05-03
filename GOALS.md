# Mercator — Goals

## Phase 1 — Make it real (2-3 weeks)

- [ ] One screen that shows every project I have (local, GitHub, GitLab, Obsidian)
- [ ] Fast search and filter across all of it (SQLite + FTS5)
- [ ] Per-project deep-dive page so I stop re-doing archaeology on context switches
- [ ] CLI access (`list`, `search`, `export`) so I'm not locked to the UI
- [ ] Markdown export — one file per project — that any other tool can consume
- [ ] Provider trait so adding new sources (Vercel, Supabase, Turso) is plug-in work, not surgery
- [ ] Dogfood for a week and decide what hurts most

## Phase 2 — Make it smart

- [ ] LLM synthesis per project ("where I left off" stops being a stub)
- [ ] Deployment status (Vercel, Supabase, Turso) on the same screen as the code
- [ ] Smarter graph — semantic edges, not keyword overlap
- [ ] Mercator export feeds my Obsidian LLM-wiki, maintained by Claude Code
- [ ] Cost visibility — at minimum dev-time AI spend per project (already in swarm logs)

## Phase 3 — Close the loop

- [ ] Swarm workflow templates (bootstrap MVP, weekly maintenance, idea→implementation)
- [ ] Trigger workflows directly from Mercator with per-project guardrails
- [ ] Mercator + wiki + swarm running as one personal AI dev platform — for me first, then everyone else

## Non-goals (on purpose)

- Not a team tool. Single-user until proven otherwise.
- Not a generic "any stack" tool. Opinionated on the indie-AI-SaaS stack I actually use.
- Not a replacement for GitHub/Obsidian/Vercel. A layer above them.
- Not a feature factory. If it doesn't change a decision I make, it doesn't ship.
