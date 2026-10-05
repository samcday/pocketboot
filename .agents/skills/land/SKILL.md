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
7. **Reconcile the initiating checkout.** This is part of the requested workflow,
   distinct from publishing upstream. In an isolated Delta Land subthread, use
   `send_agent_message` to request the parent perform the reconciliation below
   in its own attached checkout. Include the repository, upstream remote, target
   branch, landed SHA, and any known unlanded edits. Do not reach into the parent
   checkout by filesystem path. A landing card or child-to-parent file merge is
   not checkout reconciliation. If a handoff is unavailable or unconfirmed,
   report publication as landed but parent cleanup as pending; do not poll.

Use `GIT_EDITOR=true` for commits/rebases/merges; no interactive rebases. Leave
branches and unrelated work intact. Do not weaken checks or change repository
settings.

## Parent checkout reconciliation

Execute this on an explicit cleanup request or handoff, not on an informational
landing event alone. Inspect the parent's current state, not the snapshot from
when the Land subthread started; the user may have continued editing.

1. Fetch the configured upstream and record the target SHA. Confirm the landed
   SHA is reachable from it, the current branch is the target branch, and the
   current `HEAD` is its ancestor. Stop on divergence, a different branch, or
   staged changes rather than rewriting history or flattening the user's index.
2. Record status and the contents of remaining edits. Classify modified paths:
   an already-landed path must match the fetched target in full, including its
   file mode. Verify with `git diff --no-ext-diff --quiet <target-sha> -- <paths>`.
   Preserve unlanded, untracked, and ignored work. Stop if a file mixes landed and
   unlanded edits and cannot be carried through a normal fast-forward.
3. If there are already-landed modifications, save only those explicitly named
   paths with `git stash push -m "post-land reconciliation" -- <paths>`, recording
   the recovery stash's object ID. Never run this with an empty path list, and
   never blindly pop/apply that stash: its changes are already upstream.
4. Run
   `GIT_EDITOR=true git merge --ff-only --no-autostash --no-overwrite-ignore <target-sha>`.
   Disable configured autostash and refuse collisions with ignored files. Leave
   unlanded edits in place; if Git refuses, stop and retain the recovery stash.
   Do not use `reset --hard`, `clean`, blanket restores, or an automatic stash pop.
5. Verify `HEAD` equals the recorded target, already-landed paths no longer show
   as dirty, and every unlanded edit/untracked file is unchanged. Report remaining
   edits and the recovery stash separately from publication status. Keep the
   recovery stash unless its removal is explicitly requested; do not commit or
   publish leftovers just to obtain a clean diff.

## Sources of truth

- `.github/workflows/ci.yml`: PR-head checkout, authorization, and build jobs.
  `.github/workflows/ci-image.yml` and `.github/Dockerfile`: CI image/toolchain.
- `cargo xtask ci-matrix` supplies the expected matrix: see `.cargo/config.toml`,
  `xtask/src/main.rs`, and `xtask/src/commands/ci_matrix.rs`.
- Use checked-in hygiene/test definitions and applicable project instructions;
  do not invent blanket workspace/all-feature checks or waive existing failures.
