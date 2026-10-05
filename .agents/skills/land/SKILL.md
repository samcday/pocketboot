---
name: land
description: >-
  Land pocketboot changes in upstream main through an open PR, green GitHub
  Actions checks, and a fast-forward-only push. Invoke only when the user
  explicitly requests landing, not for review, preparation, or installation.
disable-model-invocation: true
metadata:
  delta-action: land
---

# Land pocketboot

The landing request authorizes execution; do not ask again whether to merge.
Review bots are advisory and handled separately, not another landing ceremony.

1. **Prepare.** Read project instructions and inspect status, remotes, and live
   repository rules. Commit the intended changes on a topic branch, preserving
   logical commits and unrelated work. Target `samcday/pocketboot`'s upstream
   `main`, never the `local` remote or the attached Linux repository. Stop for
   unclear scope, missing permissions, or unmet contribution requirements.
2. **Rebase.** Fetch upstream `main` and rebase onto it. Resolve clear conflicts
   automatically; stop for ambiguous intent, design decisions, or unsafe changes.
   Never force-push: use a fresh branch/PR if published history needs rewriting.
3. **Open a PR.** Push the topic branch normally and create or reuse an open PR
   against `main` using `gh`. Supply explicit repository, branch, title, body,
   and IDs rather than prompting. Do not require another review round.
4. **Get green CI.** Require a completed, successful GitHub Actions `CI` run for
   the exact PR head, with every expected matrix job passing. Inspect the latest
   run attempt and jobs, not just `gh pr checks --required` (which may list none).
   All other required checks, repository-defined hygiene/tests, and relevant
   smoke checks must also pass. Missing, pending, skipped required jobs, failed,
   or unverifiable checks are blockers. Follow `AGENTS.md` before local builds.
5. **Land the tested SHA.** Recheck the PR head, checks, rules, and freshly fetched
   upstream `main`. Any candidate/base change means rebase and fresh verification.
   Confirm `main` is an ancestor of the tested SHA, then use an ordinary
   `git push <upstream> <tested-sha>:refs/heads/main`. Never force or bypass rules.
   A rejected push means refresh and reverify, not override.
6. **Confirm.** Fetch and verify the tested commit is reachable from upstream
   `main`, and check GitHub's PR merge state. Report the commit, PR, and CI status
   accurately, including any pending post-push run. A branch or PR alone is not
   success. If blocked, say it has not landed; distinguish uncertain confirmation
   after a successful push from failure to publish.

Use `GIT_EDITOR=true` for commits/rebases; no interactive rebases. Leave branches
and unrelated work intact. Do not weaken checks or change repository settings.

## Sources of truth

- `.github/workflows/ci.yml`: PR-head checkout, authorization, and build jobs.
  `.github/workflows/ci-image.yml` and `.github/Dockerfile`: CI image/toolchain.
- `cargo xtask ci-matrix` supplies the expected matrix: see `.cargo/config.toml`,
  `xtask/src/main.rs`, and `xtask/src/commands/ci_matrix.rs`.
- Use checked-in hygiene/test definitions and applicable project instructions;
  do not invent blanket workspace/all-feature checks or waive existing failures.
