# ADR-0045: Forgejo tag authority, GitHub release executor

## Status
Accepted

## Context

Pali's canonical repository and merge authority live in House Forgejo. GitHub
is a public mirror, but historically a GitHub workflow created release tags
after mirrored `main` passed GitHub CI. That left two repositories capable of
originating release refs and required a GitHub App solely to make a bot-created
tag trigger downstream workflows.

Release publication still needs GitHub-native facilities for public binaries,
GHCR images, release notes, and crates.io trusted publishing.

## Decision

Forgejo is the sole release-tag authority. After a push to canonical `main`
passes Forgejo CI, the workflow derives the current package version's release
boundary from first-parent history. It creates an SSH-signed annotated
`v<package-version>` tag, with the message `pali v<package-version>`, only
when that boundary includes the version change and an exact dated
`## [<package-version>] - YYYY-MM-DD` changelog addition. The configured
mirror copies that exact tag object to GitHub.

Release tags are annotated and SSH-signed. The `release-boundary` job, which
runs the full test suite, holds no signing key: its local tag is annotated and
unsigned and only drives the exact-boundary checkout and gates. Only the
`tag-release` job receives the `TAG_SIGNING_KEY` secret. It writes the key to
a 0600 temporary file, signs with `gpg.format ssh`, and verifies the new tag
against `.forgejo/allowed_signers` from the verified release commit before
pushing, so a tag that cannot verify is never pushed. After the push it reads
the tag back from Forgejo and requires the advertised ref to be exactly the
tag object it just created (compared by object id), carrying that exact tag
name, peeling to the release commit, and with a signature that verifies
against the same allowed signers. `TAG_SIGNING` selects `ssh` (the
default) or `none` (annotated and unsigned, with a warning); any other value
fails, and there is no fallback from `ssh` to unsigned. With no key provisioned
in the secret and in `.forgejo/allowed_signers`, tagging fails closed.

The GitHub release executor applies the same rule before every publication
effect: every GitHub release workflow sets `TAG_SIGNING: ssh`, and the remote
tag check also defaults to `ssh` when it is unset. A tag pushed to GitHub by
any other path is refused unless it is a signed annotated tag object for that
name. The only exception is `release/legacy-release-tags.txt`, an audited list
of tags that predate signing (such as the lightweight `v0.19.0` and the
maintainer-signed `v0.17.2`), each pinned to the commit it resolves to: a
listed tag is accepted only at exactly that commit, which keeps those releases
repairable. Like `release/legacy-image-digests.txt`, the list changes only by
reviewed source change and never gains new releases.

The repository convention is to change the package version and matching dated
changelog entry in one reviewed PR/merge. The current reconciliation mechanism
can also recognize those changes when they arrive in separate pushes before a
later green run; it does not durably record original push boundaries. Enforcing
same-push provenance requires a future durable release-intent queue and is not
claimed by this workflow.

Tagging reconciles durable canonical state rather than only the latest commit
edge. If the version-bump run fails before its tag is pushed, a later green
`main` push finds the commit where the current version and its exact changelog
heading entered first-parent history. It tags the first commit at or after the
version entry that adds the matching heading, after proving that boundary is an
ancestor of the newly tested green `main`. Thus a version commit followed by a
changelog commit is supported, while later unversioned product fixes are not
silently published under the old version. Before the tag is pushed, Forgejo
checks out that exact boundary and reruns the complete formatting, workflow,
lint, test, and dependency-audit gate. A boundary that needs a later product
fix cannot release under its old version; it requires a new version/finalization
boundary. An existing matching tag is preserved; an existing conflicting tag
fails closed. Matching means that the tag peels to the exact derived
version-plus-changelog boundary, not merely any same-version ancestor.

Release versions must increase strictly under SemVer. A downgrade, repeat, or
prerelease transition that does not advance the previous package version fails
closed before tag creation.

Distinct canonical commits and distinct mirrored tags use distinct workflow
concurrency identities, so a later pending run cannot replace an earlier
release event. The releasable SemVer dialect is lowercase because GitHub folds
concurrency-key case. Push publication and manual repair are different event classes
and therefore also use distinct concurrency identities. GHCR publication
creates immutable full-version tags only and explicitly disables metadata
action's automatic `latest` flavor. The former mutable `latest` and major/minor
tags are intentionally not updated: removing that second ordered state avoids
concurrent releases rolling a floating tag back.
Until a durable ordered release queue exists, maintainers merge one version
bump at a time and wait for its Forgejo tag and GitHub release readback before
merging the next. The immutable full-version tags prevent rollback of a
floating alias, but this operational gate remains necessary for ordered notes.
Ordinary GitHub CI builds a local `butterflyskies/pali-ci` image for validation
but receives no package-write credential. Only the protected release
environment may publish `butterflyskies/pali`.

GitHub does not create tags. A mirrored tag triggers the GitHub release
workflow, which remains the public artifact executor. Release jobs are safe to
rerun: an exact existing GitHub release is reused, and an exact existing
crates.io version skips only crate publication. Authentication, transport, and
all other publication failures remain fatal. Stable manual repair never
mutates an image and may update notes or undraft only after a no-mutation probe
binds its immutable full-version image tag to the release source label. A
reviewed `release/legacy-image-digests.txt` entry supplies the exact manifest
digest only for named historical images that predate source labels. The
preflight emits that verified digest and the undraft boundary requires the
same digest again; mere tag existence is never sufficient.

Binary archives use a stable member order, release-commit timestamp, numeric
ownership, and timestamp-free gzip header. Existing archive/checksum pairs are
preserved only when internally valid and byte-identical to the freshly prepared
pair. A mismatch fails closed for separately authorized investigation rather
than blessing or silently replacing different bytes; either exact half of a
partial pair may be repaired without mutating the half already present.

GitHub must protect `refs/tags/v*` with a ruleset that permits creation only by
the configured Forgejo mirror identity and prohibits update or deletion. The
workflow additionally requires the release commit to be contained in GitHub's
exact remote default branch before every publication effect. That containment
check rejects off-main tags, but repository code cannot prove who created an
otherwise-valid tag; the external ruleset is an activation requirement, not an
in-repository guarantee.

Activation also requires two effect-bound authority gates:

- the Forgejo `tag-release` job's token must have permission to create tags in
  `lacuna/pali`, while all other workflow jobs remain read-only; activation is
  complete only after a canary tag push is read back as the exact intended
  annotated tag object (the advertised object id equals the tag just created)
  whose SSH signature verifies against `.forgejo/allowed_signers`, and
  unauthorized workflow identities are denied.
  The `TAG_SIGNING_KEY` secret and its public key in
  `.forgejo/allowed_signers` must both be provisioned first;
- GitHub's `release` environment must admit mirrored `refs/tags/v*` and
  default-branch manual repair runs, deny every other ref, and require the
  configured reviewers/branch policy before its jobs receive publication
  credentials. A successful admitted canary and a denied off-policy canary are
  required readbacks; workflow YAML alone cannot prove that external policy.
- GHCR package-write authority for `butterflyskies/pali` is granted only to the
  protected release workflow through its automatic `GITHUB_TOKEN` with
  `packages: write`; ordinary CI remains read-only and cannot publish. The
  release environment's ref policy and an admitted publication/readback canary
  are activation requirements. Stored source-revision labels are reconciliation
  evidence inside that protected workflow boundary; they are not signatures.

## Consequences

- Canonical merges and release refs share one authority boundary.
- Forgejo CI gates tag creation; GitHub CI is no longer a second tag gate.
- New release tags are SSH-signed annotated tag objects; releasing requires a
  maintainer-provisioned `TAG_SIGNING_KEY` secret and matching
  `.forgejo/allowed_signers` entry, and fails closed without them.
- GitHub continues to publish public release artifacts from mirrored tags.
- Release-image consumers pin full versions; the workflow does not maintain
  mutable `latest` or major/minor aliases.
- Ordinary CI validates `butterflyskies/pali-ci` locally and does not publish.
- The Forgejo mirror configuration is operationally required to copy tags.
- The Forgejo release workflow's tag-push authority and its exact-ref readback
  are activation requirements.
- GitHub's mirror-only `v*` tag ruleset must be active before enabling release
  publication.
- GitHub's `release` environment ref policy must be configured and canary-read
  back before publication is enabled.
- The protected release workflow's GHCR write/readback path must be canary-read
  back before immutable-tag source labels can authorize retry promotion.
- Existing-release probes become part of the release workflow's trusted
  control path and must fail closed on ambiguous responses.
- ADR-0018's dedicated release-manager App decision is no longer active for
  Pali's current release topology.
