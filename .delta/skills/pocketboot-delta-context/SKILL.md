---
name: pocketboot-delta-context
description: Use this thread's attached kernel for pocketboot builds; recheck when attachments or machines change.
user-invocable: false
---

# Pocketboot Delta context

Make builds in this pocketboot worktree use the attached kernel worktree, if
there is one: write its absolute path to `.localkernel` in pocketboot. Clear
any stale binding when no kernel is attached.
