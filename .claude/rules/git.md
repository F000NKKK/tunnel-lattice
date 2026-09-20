# Git rules

The destructive-command restrictions below are also enforced mechanically
via `permissions.deny` in `.claude/settings.json`, not just as prose here.

- Inspect `git status --short` and `git diff --check` before and after each
  bounded task.
- Preserve unrelated user changes; never use `git reset --hard`, `git
  clean`, or broad checkout commands.
- Do not amend, rebase, or force-push unless the user asks for that specific
  Git operation.
- Auto-commit is authorized standing policy for this repository: after each
  bounded slice (a YouTrack Task, or any reviewer-accepted edit) reaches a
  clean, verified state (relevant `cargo check`/`test`/`fmt`/`clippy` gates
  pass and the evidence comment is posted per `@.claude/rules/youtrack.md`),
  create a commit for the changed tracked files without asking first. Stage
  narrowly (named paths, never `git add -A`/`.`), keep the message focused
  on the "why" per the root git guidance, and never bypass hooks
  (`--no-verify`) to force one through.
- Commit messages (auto-commits and user-requested commits alike) must NOT
  include a `Co-Authored-By` trailer or any other attribution footer. This
  overrides the default Claude Code commit template for this repository.
- Every commit (auto-commit or user-requested) MUST name the `TL-*` issue
  ID(s) it relates to, AND MUST use this exact subject-line format so both
  the repo's Conventional-Commits-based release tooling
  (`scripts/release.sh` emits `chore(release): bump N crate versions`
  commits in this exact format) and the release-search script that groups
  commits by `TL-*` ID can parse it:

  ```text
  <type>/TL-<id>: <summary>
  ```

  `<type>` is a standard Conventional Commits type (`feat`, `fix`, `docs`,
  `refactor`, `chore`, `perf`, `test`, `ci`, `build`) chosen for what the
  commit actually does — do not default to `feat` for a docs-only or
  refactor-only change. `<id>` is the primary `TL-*` issue this commit
  implements (e.g. `feat/TL-46: add DesiredState aggregate type`). If one
  commit spans more than one Task, use the primary Task's ID in the subject
  and mention every other relevant `TL-*` ID in the body. A breaking change
  still uses the type's own `!` marker before the slash if the project's
  Conventional Commits convention calls for one (e.g. `feat!/TL-42: ...`).
  This is mandatory on every commit, not optional polish: it is how a diff
  traces back to its authorizing Task/Story/Epic/Bug, feeds the
  release-search script, and the GitHub repository is linked to YouTrack's
  VCS integration so a referenced ID also auto-links the commit to that
  issue's activity feed. Do not use YouTrack command keywords (e.g.
  `fixes TL-13`, `closes TL-13`) that could auto-transition the issue's
  `Stage` — Stage transitions in this workflow are role-gated (reviewer
  confirmation required before `Done`, see `@.claude/rules/youtrack.md`) and
  must stay explicit `mcp__youtrack__update_issue` calls, not a side effect
  of a commit message.
  - This format applies from this file's creation onward. Do not
    amend/rewrite an older commit retroactively to match it (that requires
    explicit user authorization per the destructive-operation restrictions
    above).
- If the change implements or follows a decision recorded in a YouTrack
  knowledge-base Article (an ADR under `TL-A-1`, e.g. `TL-A-7`), also name
  that Article ID in the commit message alongside the issue ID(s) — a diff
  that exists because of an ADR must be traceable back to it from the commit,
  not only from the issue description.
- Auto-commits still respect every other restriction in this file: no
  amend/rebase/force-push, no destructive staging, no committing `.codex/`,
  root `AGENTS.md`, or `index.md`.
- Keep patches narrow and reviewable. Separate model/API, backend, tests,
  and documentation changes when practical.
- `.codex/`, root `AGENTS.md`, and `index.md` may be intentionally ignored
  local agent context; inspect them but never force-add them. Do not change
  ignore policy unless the user explicitly asks.
- Before handoff, report changed tracked files, verification commands, and
  any privileged tests not run locally.
