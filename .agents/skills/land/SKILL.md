---
name: land
description: >-
  Land explicitly requested pocketboot changes through reviewed pull requests,
  public CI boot images, and verified hardware evidence, while preparing any
  accompanying Linux work as a self-contained, bisectable b4 series for human
  submission. Invoke only when the user explicitly requests landing or merging;
  review, preparation, passing checks, and skill installation are not landing
  requests. Applies to samcday/pocketboot and its associated upstream Linux work,
  not to unrelated repositories.
disable-model-invocation: true
metadata:
  delta-action: land
---

# Land pocketboot changes and prepare the associated Linux submission

## Contract

An explicit invocation supplies landing intent. Proceed through preparation,
verification, and the pocketboot merge without asking for that same permission
again. Stop for genuine blockers, missing human contributions, ambiguous scope,
or unsafe operations—not for routine reconfirmation.

Publishing a draft PR or candidate image is useful progress, but is not landing.
Complete this workflow by merging the accepted pocketboot PR into
`samcday/pocketboot:main` and verifying the destination. Preserve any associated
Linux work as an individually committed, send-ready submission series. Do not
merge that series into a Linux default branch or an upstream maintainer tree.

Do not send email, flash devices, publish releases/packages, change repository
protections, force-push, or overwrite snapshot tags. Those are not authorized by
this workflow. In particular, `b4 send --reflect` and `--preview-to` send mail;
they are not dry runs.

For pocketboot-only changes, omit the Linux submission steps. Keep verification
proportional: a skill/documentation-only change does not manufacture a new
hardware-test requirement. If the request contains no pocketboot change to
merge, identify that scope mismatch rather than calling preparation “landed.”

## 1. Establish the change and destinations

- Read applicable instructions and current contribution requirements. Inspect
  status, diffs, history, remotes, available tools, authentication status, and
  destination settings without exposing credentials.
- Identify the intended changes, device/profile, pocketboot branch, any Linux
  topic, public upstream base, and declared prerequisites. Preserve unrelated
  local work; do not use blanket staging, reset, clean, or stash operations.
- Prepare coherent commits for the intended changes, including relevant
  uncommitted work. Keep exploratory instrumentation, private captures, firmware
  backups, secrets and unrelated fixes out of the submission.
- Verify publication remotes resolve to the user's repositories. The expected
  repositories are `samcday/pocketboot` and `samcday/linux`; `local` is not a
  publication remote. Stop on an unexplained ownership or destination change.
- Recheck `main` rules, permissions, required checks, review requirements and
  allowed merge methods at execution time. Existing history is not a substitute
  for policy. Do not assume a merge queue or branch protection exists.
- Select one evidence-note publisher. Fetch current `refs/notes/evidence`
  before changing it; preserve others' notes and forward-only privacy fixes.
  Never force-push this ref or blindly reintroduce redacted predecessor text.

Use non-interactive Git operations. Prefix commands that can invoke an editor
with `GIT_EDITOR=true`; supply commit/tag messages explicitly. Resolve clear,
mechanical conflicts automatically while preserving intended and unrelated
changes. Pause on semantic ambiguity, incompatible intent, or any operation
requiring an otherwise unauthorized history rewrite.

## 2. Curate the Linux series

The kernel series must stand on its declared public base, not on an accidental
worktree state or an undocumented private fork.

- Separate logically reviewable changes. Put bindings before their consumers
  and DTS changes after required bindings/driver changes. Remove fixup/WIP
  history without collapsing independently meaningful patches into one blob.
  An intentional empty b4 cover commit is metadata, not an implementation patch.
- Account for every kernel modification used by the test image. Inspect the
  pocketboot source pin and its patch list; either include prerequisites in the
  series or declare and pin them explicitly. Hidden carried patches invalidate
  a claim that the image tests the exported upstream series.
- Preserve authorship and existing legitimate trailers. Verify hardware claims,
  not merely schema syntax: successful operation does not prove an invented
  regulator topology. Omit or resolve unsupported descriptions.
- Prefer a fresh local curation branch to rewriting a shared branch. If an
  existing remote branch cannot be updated normally, use a new revision branch
  or stop for specific rewrite authorization. Do not force-push implicitly.
- Record a range-diff from the previously reviewed/tested revision. Changes to
  code, configuration or build inputs invalidate affected checks and tests.
  For metadata-only restacks, record tree equivalence and the actual tested
  revision; never relabel an untested binary as the tested artifact.

Sources:
- Linux `Documentation/process/submitting-patches.rst` and
  `Documentation/devicetree/bindings/submitting-patches.rst`.
- Linux `Documentation/process/maintainer-soc.rst` for platform validation.
- Pocketboot `xtask/src/commands/config.rs` (`KernelSourceLayer`,
  `load_device_config`) and `xtask/src/commands/kernel_src.rs`
  (`ensure_device_kernel_source`, patch application).

### Human ownership and b4 metadata

Follow Linux `Documentation/process/coding-assistants.rst` and
`Documentation/process/generated-content.rst`, including their linked process
documents when applicable. Humans must understand and take responsibility for
the complete submission.

- Obtain the authors' factual rationale, supported scope, limitations and test
  statements. Where policy or the user requires human-authored text, obtain that
  text; generating it and receiving approval is not equivalent.
- Never add a human's `Signed-off-by`. Preserve legitimate existing sign-offs;
  require humans to add their own final certification. Do not infer certification
  merely from Git identity, co-authorship, or a previous review approval.
- Keep applicable `Co-developed-by`/sign-off ordering and assistance disclosure.
  Obtain permission for test/review trailers and establish which revision they
  cover. Do not fabricate them from a successful tool result.
- If b4 tracking is absent, have a human initialize/enroll the curated series.
  Installed b4 0.14.3 creates a cover sign-off during new-series/enrollment setup;
  do not automate that certification on the human's behalf.
- Preserve b4's change ID and mail revision across iterations. Snapshot build
  numbers are not mailing-list revision numbers.

Once tracking and human-supplied content exist, these installed b4 interfaces
are available:

```sh
b4 prep --show-info
b4 prep --auto-to-cc
b4 prep --check-deps
b4 prep --check
b4 prep --format-patch "$EXPORT/patches"
b4 send --output-dir "$EXPORT/mail"
```

The last command forces a dry run and writes messages; it does not send them.
Inspect the generated messages, recipients, base information, dependencies and
trailers. Do not regard a successful export as proof that all review/test gates
passed. Do not invoke an interactive cover editor.

Tool references: installed `b4 prep --help` and `b4 send --help`; the sign-off
behavior was checked in b4 0.14.3's `b4.ez.start_new_series`. Recipient and patch
requirements are defined by Linux `MAINTAINERS`,
`scripts/get_maintainer.pl`, and `Documentation/process/submitting-patches.rst`.
Recheck supported interfaces if the installed b4 version differs.

## 3. Verify the series, not only its tip

Use a clean, dedicated validation branch in the attached Linux checkout, with
all intended work safely committed and unrelated work preserved. Keep generated
output outside the source files. Do not operate on another person's checkout.

Apply the exported numbered code patches, excluding the cover letter, in order
to the declared base plus declared prerequisites. Use non-interactive
`GIT_EDITOR=true git am`. Validate each prefix and compare the final tree with
the frozen submission tree. Restore the original branch on completion; abort
only validation operations started by this workflow on failure.

- Run checkpatch against every implementation commit.
- Validate changed bindings and affected DTS files. A binding-only prefix need
  not contain a board file introduced by a later patch.
- Build affected code/configurations at each applicable prefix. Driver changes
  need their relevant build/test coverage, not only DT validation.
- Compare warnings against the base. No new unexplained warnings or errors.
  A validator traceback or skipped tool is not success just because `make`
  returned zero. Document justified inherited warnings or false positives;
  never silently suppress required checks.
- Verify the final clean tree can produce the target pocketboot image through
  the checked-in recipe.

For the present arm64 board series, the supported kernel interfaces include:

```sh
./scripts/checkpatch.pl --strict --git "$BASE..$KERNEL_SHA"
make O="$KOUT" ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- defconfig
make O="$KOUT" ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- \
    DT_SCHEMA_FILES=arm/qcom.yaml dt_binding_check
make O="$KOUT" ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- \
    CHECK_DTBS=y DT_SCHEMA_FILES= qcom/msm8939-xiaomi-ferrari.dtb
```

Adapt architecture, toolchain, binding selection and DTB target to the actual
change—not by blindly running Ferrari targets for another device. The final
DTB check must not accidentally inherit a filter that excludes its bindings.

Sources: Linux `scripts/checkpatch.pl` option definitions; root `Makefile`
targets `dt_binding_check`, `dtbs_check` and `%.dtb`;
`Documentation/devicetree/bindings/Makefile` (`DT_SCHEMA_FILES`);
`scripts/Makefile.dtbs` (`CHECK_DTBS`); and the relevant DTS Makefile entry.
Tool requirements come from Linux `scripts/min-tool-version.sh`, the DT schema
Makefile and pocketboot's current `.github/Dockerfile`, not executable presence
alone.

Pocketboot's normal verification/build interfaces are:

```sh
cargo test --locked -p xtask
cargo xtask ci-matrix
cargo xtask build "$DEVICE" "$KERNEL_TREE"
```

Run additional Rust, helper or regression tests appropriate to changed code.
Avoid rebuilding unrelated devices locally when CI covers them. An offline
run is a cache-dependent convenience, not a reproducibility guarantee.

Sources: workspace and `xtask/Cargo.toml`; `.cargo/config.toml`'s `xtask` alias;
`xtask/src/main.rs` subcommands; `xtask/src/commands/build.rs` positional
`VENDOR/DEVICE` and `KERNEL_TREE` arguments; the tests in
`xtask/src/commands/config.rs`; and `xtask/src/commands/ci_matrix.rs`.

## 4. Publish identified draft cycles for the public

Open or update draft review PRs on the verified user-owned repositories early
enough for CI and review to guide iteration. Kernel PRs are review surfaces,
not requests to merge Linux into the fork's default branch.

- Choose a kernel review base whose PR diff matches the intended series. A
  stale or unrelated fork base is not permission to update its default branch.
- Pin pocketboot's `[kernel-source]` to the exact published kernel commit and
  check the complete applied patch set before committing the testbed revision.
- Supply explicit repo, base, head, title and body to `gh pr create`; do not
  permit implicit forking, destination selection or an interactive editor.
  PR descriptions contain conclusions and public evidence pointers, not raw
  private captures or a transcript dump.
- Publish immutable snapshot tags in each participating repository using
  `b4/<topic>/<version>`, for example `b4/ferrari/v1-test.1`. Increment the
  snapshot version for the next cycle; never move, delete or reuse an existing
  tag for different contents. Verify local and remote tag targets.
- Record both repositories' tagged commits and their relationship in a cycle
  manifest. A tag identifies a candidate; it does not itself certify CI,
  hardware success, or mailing-list readiness.
- Export the corresponding b4 patch/message bundle for each qualified cycle.
  Preserve the public source refs needed to regenerate it and the checksums of
  the actual exported files.

Normal tag and branch publication is allowed within these verified user-owned
repositories. Respect any signing policy and stop if signing cannot be completed
non-interactively. Snapshot tags are not automatic GitHub releases.

Example PR interface, after branches are explicitly published:

```sh
gh pr create --repo "$P_REPO" --base main --head "$P_BRANCH" \
    --draft --title "$TITLE" --body-file "$BODY"
```

The analogous kernel PR uses its separately verified repository/base/head.
`gh pr create --help` establishes these non-interactive options.

## 5. Require substantive CodeRabbit review and successful CI

For both relevant PRs, bind review and checks to the current frozen head.
Inspect repository-required checks as well as this workflow's explicit gates.
An unprotected branch or an empty `--required` result does not waive them.

CodeRabbit is an external GitHub integration here, not a checked-in workflow.
Verify an actual completed review of the current revision and inspect its
findings/pre-merge checks. A successful status with a “review skipped” comment
does not pass: this has occurred on pocketboot because automatic review was
disabled for repositories with fewer than ten stars.

Request a complete review through the documented manual trigger when needed:

```sh
gh pr comment "$PR" --repo "$REPO" --body '@coderabbitai full review'
```

For later changes, the documented `@coderabbitai review` command requests an
incremental review. In either case, verify the resulting review covers the
current revision; posting the request does not satisfy the gate.
Source: CodeRabbit's
[review command reference](https://docs.coderabbit.ai/reference/review-commands).
If the integration cannot perform the requested review, report the blocker.
Do not invent bot commands, change account plans or add repository stars.

Address actionable CodeRabbit and human findings. Record a reasoned human
disposition for false positives rather than pretending the bot never raised
them. Do not bypass a required failing check or resolve a discussion merely
to hide an outstanding issue.

Pocketboot CI must produce the relevant device artifact from the frozen
pocketboot revision and the declared kernel pin. Verify that the generated
matrix includes the device with `bootimg: true` and the expected kernel SHA.
Wait for all applicable required checks to finish successfully; pending,
cancelled, missing, skipped-required or unverifiable checks block landing.

```sh
gh pr checks "$P_PR" --repo "$P_REPO" --watch --interval 15 --fail-fast
```

Use bounded waits and re-query the PR head and expected check set afterward.
Starting checks, watching only some jobs, or a zero exit status on an incomplete
check set is insufficient. If the head changes, reassess the affected gates.

Sources: `.github/workflows/ci.yml` defines the PR triggers, authorization gate,
matrix, kernel/boot-image jobs and upload; `.github/workflows/ci-image.yml`
defines the image prerequisite and `ci-ok` condition. Do not self-approve
untrusted PR code by adding `ci-ok`; obtain the required maintainer authorization.
Current `gh pr checks --help` supplies the watch flags and pending exit code.

## 6. Deliver and qualify the exact CI image

Select the successful workflow run by repository, run ID, attempt and head SHA;
never use an unqualified “latest artifact.” Check expiration and that the run
actually executed the expected build.

The artifact name is `bootimg-` plus the device ID with separators sanitized to
hyphens. For Ferrari it is `bootimg-qcom-msm8939-xiaomi-ferrari`, containing
`boot.img`.

Resolve the selected attempt explicitly:

```sh
gh api "repos/$P_REPO/actions/runs/$RUN_ID/attempts/$RUN_ATTEMPT"
gh api --paginate \
    "repos/$P_REPO/actions/runs/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100"
```

Verify the attempt's `head_sha` is the frozen pocketboot commit and that the
expected device job and its `Upload boot image` step completed successfully.
Take `JOB_ID` from this attempt-specific job list, then capture its log:

```sh
mkdir -p "$OUT/image"
gh api --allow-escape-sequences \
    "repos/$P_REPO/actions/jobs/$JOB_ID/logs" > "$OUT/upload-job.log"
```

Obtain `ARTIFACT_ID` from that upload step's recorded artifact ID/download URL,
or an equally trustworthy attempt-specific upload manifest. Do not infer it
from a name match, the newest creation time, or a run-wide artifact list.
`gh run download RUN_ID --name NAME` does not select a run attempt and must not
be used to resolve ambiguous same-name artifacts. If the attempt-to-artifact
association cannot be established, stop rather than qualify an uncertain image.

Verify the exact artifact record: expected name, `expired: false`,
`workflow_run.id == RUN_ID`, and `workflow_run.head_sha` matching the frozen
commit. Record its archive digest, then download by immutable artifact ID:

```sh
gh api "repos/$P_REPO/actions/artifacts/$ARTIFACT_ID" > "$OUT/artifact.json"
gh api --allow-escape-sequences \
    "repos/$P_REPO/actions/artifacts/$ARTIFACT_ID/zip" > "$OUT/artifact.zip"
sha256sum "$OUT/artifact.zip"
```

Compare the downloaded archive hash with the artifact record's SHA-256 digest
before extraction. Inspect archive entries and extract only the expected
`boot.img` into the fresh output directory; reject unexpected paths or duplicate
entries. Then compute the extracted image's own SHA-256. Preserve the
run/attempt/job/artifact-ID association and both hashes in the cycle manifest.
The escape-sequence flag permits raw log/archive bytes only into files; do not
display untrusted terminal control sequences.

Sources: `xtask/src/commands/ci_matrix.rs` constructs the artifact name;
`.github/workflows/ci.yml` uploads
`target/kernel/${DEVICE}/boot.img`; GitHub's
[attempt-specific jobs API](https://docs.github.com/en/rest/actions/workflow-jobs#list-jobs-for-a-workflow-run-attempt)
and [artifact API](https://docs.github.com/en/rest/actions/artifacts#download-an-artifact)
define the lookup and ID-based download. These commands were checked with
GitHub CLI 2.97.0. Verify supported interfaces on other versions without dropping
the attempt-binding gate. GitHub's archive digest is not the extracted
`boot.img` digest.

Provide the public with:

- The public draft PR, exact CI run/attempt and artifact links, and upload job ID.
- The extracted image's size and SHA-256.
- Kernel base, prerequisites, series/snapshot and tested kernel commit.
- Pocketboot snapshot/commit, device configuration and build-environment
  identity, including the resolved CI image digest where available.
- The exact validated bootloader prerequisite and transient boot command.
  For this Ferrari path, do not imply stock-bootloader support merely because
  `fastboot boot` works through lk2nd.
- A short test checklist, known limitations and evidence-note pointers.
- Artifact expiry and any GitHub-login requirement. A public PR does not make
  its CI artifacts permanent, anonymous release downloads.

If an artifact expired, rebuild from the frozen inputs and re-qualify its new
digest; do not assume it is byte-identical. If anonymous/permanent hosting is
required but not already available, stop for an authorized publication plan.
Do not silently create a release or upload to a third-party service.

The current Dockerfile pins its base image but resolves some packages/tools
during construction. Record actual inputs and environment; do not call this
hermetic or byte-reproducible solely because CI passed. Such a claim requires
appropriately pinned/isolated inputs and independent clean builds with matching
output hashes. Toolchain pinning alone does not settle timestamps, build paths
or other nondeterminism. Treat that improvement as separately reviewed work.

Keep the PR draft while qualification is incomplete. Obtain the session's agreed
hardware evidence against the identified artifact. For this bring-up, that
means the claimed framebuffer/USB behavior on the agreed devices—not merely
upload success, a static bootloader splash, or an old differently configured
image. Separate described, measured, tested and untested features.

Do not operate a phone as a side effect of landing. Supply the validated command
to its operator, with an explicitly selected device, and collect results.

## 7. Land pocketboot and verify the result

When all applicable contribution, review, CI and hardware gates pass:

1. Verify the PR still has the frozen tested head and an up-to-date, tested
   relationship to `main`. Refresh stale bases safely and repeat affected
   gates rather than merging an untested combination.
2. Mark the pocketboot PR ready and verify any newly triggered required reviews
   or checks. Do not treat draft publication as acceptance.
3. Recheck that the destination still allows the approved squash-merge route.
   If protections or a merge queue now require a different workflow, stop
   rather than bypassing them with admin privileges.
4. Squash-merge the pocketboot PR, matching the verified head:

   ```sh
   gh pr merge "$P_PR" --repo "$P_REPO" --squash \
       --match-head-commit "$P_SHA" --subject "$SUBJECT" --body-file "$BODY"
   ```

5. Verify the PR is merged into the intended repository/branch; fetch the
   destination and verify the returned merge commit is present. Compare its
   source tree with the accepted candidate. Report any unexpected difference
   truthfully; do not silently repair, revert or force-update the destination.
6. Record the reviewed/tested and landed commit identities. Reattach applicable
   evidence notes to the squash commit and cross-reference the Linux series.
   Do not relabel a newly rebuilt, untested image as the original tested image.
7. Confirm the final b4 bundle, recipients, base/dependencies and human trailers
   remain consistent with the accepted series. Leave it ready for human sending,
   with the actual verification/coverage stated.

The merge interface and head-match safeguard are defined by `gh pr merge --help`;
the target and allowed methods must be verified against live repository settings.
Never use `--admin`, force a protected branch, or delete immutable snapshot refs.

## Completion and blockers

On success, report the pocketboot PR and landed commit, final Linux series/base
and snapshot, b4 export location/checksums, exact public CI artifact and boot
instructions, checks, hardware coverage, and evidence-note refs.

If a blocker prevents merging, explicitly say **not landed**, identify the
blocker, and preserve useful draft PRs/artifacts for the next iteration. If a
merge occurred but destination verification failed, report that partial state
accurately instead of claiming either a clean success or that nothing landed.

Raw evidence belongs in `refs/notes/evidence`, attached to the commits it
concerns and cross-referenced across repositories. Public notes use neutral
device labels and omit private backup paths, serials and credentials. PRs and
commit messages carry the conclusions and pointers. Notes must be published
with their source snapshots, without overwriting concurrent or sanitized work.
