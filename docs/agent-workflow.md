# Agent workflow

Required by [AGENTS.md](../AGENTS.md) before any tracker mutation, claim
decision, or session-attribution question. Read once before applicable work.

## Work tracking

Work is tracked in beads (`br`). `.beads/` is committed; its JSONL export is the
shared record across sessions, agents, and adapters. Never parse
`.beads/*.jsonl` directly and never initialize Beads implicitly — if the
database is missing or broken, stop and ask.

When asked to recommend or list the next best task(s), that is a read-only
triage answer: report issue ID(s), priority, and one-line reasoning, then stop.
Do not claim, implement, or commit work unless asked to proceed.

- Run `br robot-docs guide` for installed command syntax. Use the bounded query
  recipes in [AGENTS.md](../AGENTS.md) before loading full issue records.
- Discover work with `br ready` or `br scheduler`. Triage with `bvr --robot-*`
  flags only; bare `bvr` opens a blocking TUI. `bvr` is beads_viewer_rust, a
  superset of the older Go `bv` — this project does not use `bv`.

## Actor attribution

Pass `--actor "<your session id>"` to **every** mutating `br` command —
`create`, `update`, `close`, `delete`, `comments add`, `dep add`, `label add` —
not just claims. The actor is your adapter's session identifier, and it must
point at a transcript someone can actually open.

- Claude Code: use `claude/<CLAUDE_CODE_SESSION_ID>` when that variable is
  present. `--resume`/`--continue` refer to persisted sessions; do not replace
  the current session ID with one copied from a session picker.
- ACP sessions driven through this adapter (and other ACP adapters): use the
  session id your own interaction exposes. Codex exposes its id as
  `CODEX_SESSION_ID` in the shell it spawns for a tool call, not in its own
  process environment.
- Any other adapter: use the exact ID exposed by the current interaction. A
  historical `deepseek/<id>` actor in an old record proves only the stored
  shape, not your identity.

Do not use the OS username, `br config`'s `_computed.actor`, the br skill's
`BR_ACTOR:-assistant` fallback, `robot-docs`' `$AGENT_NAME`, a process name, or
a PID. Bare UUIDs and `assistant` actors are legacy records, not valid sources
for a new mutation. If no recipe yields an attributable current ID, stop and ask
the maintainer rather than falling back to one.

## Claims

Claim work with `br update <id> --claim --actor "<your session id>"`, never a
bare `--status=in_progress`. The claim is what makes concurrent agents safe.

- Finish your own claimed work before taking new work.
- Another agent's live claim is protected. Do not take over a claimed issue, and
  do not "fix" work in flight under someone else's claim — report it instead.
- If you abandon work, release or comment on the claim rather than leaving a
  silent stale assignment.

## Filing and closing

- File defects before fixing them, preserving the failing evidence — the command
  run, the observed output, the expected output. A bug whose reproduction lives
  only in a chat transcript is not tracked.
- Record unfinished or deferred work as issues before handoff, including work
  you decided not to do and why.
- Use dependencies (`br dep add <issue> <depends-on>`) for actual blockers, not
  for ordering preferences.
- Priorities are numeric: P0 critical, P1 high, P2 medium, P3 low, P4 backlog.
  Types: task, bug, feature, epic, chore, docs, question.
- Search prior lessons with `br search -a`; closed issues are otherwise omitted
  from results. `br list` does not return comments — use `br show` or
  `br comments list`.
- Before closing a bug/task/feature, route the lesson somewhere durable: the
  issue itself, a required instruction file, or a `needs-design` question. If no
  lesson is warranted, say so explicitly rather than closing silently.
- State what evidence closes the issue: a passing gate, a consumer that now
  works, a live reproduction the maintainer confirmed, or an explicit "no
  verification possible, follow-up filed".

## Session protocol

```sh
git status --short                 # account for every path, including unrelated ones
br sync --flush-only               # export the DB to JSONL before staging
git add <named paths>              # never `git add .` or `git add -A`
git commit                         # reference the issue id in the body: Refs daa-xxxx
```

Run `br sync --flush-only` before any authorized commit so the JSONL export
matches the database. Stage only paths you changed; unrelated dirty files belong
to someone else's work.
