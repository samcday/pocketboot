---
name: land
description: >-
  Land pocketboot changes in upstream main through an open PR and green GitHub
  Actions checks. Invoke only when the user
  explicitly requests landing, not for review, preparation, or installation.
disable-model-invocation: true
metadata:
  delta-action: land
---

# Land pocketboot

Preparation:

 * Ensure all new commits have a valid `Signed-off-by` tag.
 * Ensure all new commits have a `Assisted-by` tag.
 * Ensur `cargo clippy` and `cargo fmt` are clean.
 * Ensuring source branch is rebased on latest target (ensure remote is fetched).
 * Agent may handle all rebasing and stop only for confirmation on significant conflicts).

Landing procedure:

 * Open a PR.
 * Explicitly ask for a "@coderabbitai review"
 * Wait for all CI to pass for the PR.
 * Fix CI failures and address PR feedback as it is provided.
 * If changes are simple and/or low risk, or user requested it: queue the PR for automatic merge.
 * Wait for confirmation (on the PR or in the thread) that a build with the latest changes has been tested on an affected device, if appropriate.
 * Rebase on the new upstream SHA when PR has merged. Send a message to parent Delta thread to attempt the same (and have it broadcast to other subthreads that are carrying their own diff).
