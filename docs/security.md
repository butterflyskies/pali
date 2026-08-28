# Security

## Security boundaries

Pali keeps inference and derived search indexes local. Memories remain
plaintext Markdown in the managed git repository, and their content leaves the
machine only when an operator deliberately configures and invokes GitHub sync.

Namespace scopes organize retrieval; they are not access-control boundaries.
The MCP HTTP endpoint does not authenticate clients, so keep the default
loopback binding or place a remotely exposed endpoint behind an authenticated
TLS gateway and appropriate network policy.

## Credentials and local files

GitHub tokens are resolved from `PALI_GITHUB_TOKEN`, an owner-readable
token file, or the system keyring. Kubernetes deployments can inject a token
from a Secret through the environment variable. The CLI does not accept tokens
as arguments, and credentials are not written to the memory repository. On
Unix, the file store creates its directory with mode `0o700` and token file
with mode `0o600`; Pali also sets process umask `0o077` before creating
files so group and other permissions are masked off by default.

## Input and repository safety

Memory names, content size, and nesting depth are validated at input
boundaries. Content is capped at 1 MiB and names at three path components.
Repository access rejects path traversal and symlink-based escapes, including
no-follow file opens on supported Unix targets. These checks protect the
managed repository; they do not turn scopes into authorization policy.

## Container hardening

The provided image runs as the non-root `pali` user (UID 1000). The
provided Kubernetes deployment adds controls that the image alone cannot
enforce: `runAsNonRoot`, a read-only root filesystem, disabled privilege
escalation, all Linux capabilities dropped, the `RuntimeDefault` seccomp
profile, and separate runtime and token-bootstrap service accounts. Review and
preserve those settings when adapting the manifests.

The persistent memory repository and any writable caches still need writable
volumes with deployment-appropriate access controls. See
[Deployment](deployment.md) for the concrete manifests and operational notes.

## Supply-chain evidence and audit

External GitHub Actions used by the repository are pinned to full commit SHAs.
CI-published GHCR images are accompanied by BuildKit provenance in the SLSA
predicate format and by SBOM attestations; this does not claim a particular
SLSA level, and local `docker build` output is not attested. Treat those
attestations as evidence to verify, not as a substitute for registry policy or
image-signature enforcement.

CI runs `cargo deny check` against RustSec advisories and the repository's
license, ban, and source policies. Release binaries are built with
`cargo-auditable`, embedding dependency metadata for later inspection. The
implementing controls are visible in the
[CI workflow](../.github/workflows/build.yml),
[image publishing workflow](../.github/workflows/publish-image.yml), and
[`deny.toml`](../deny.toml).

## Known limits

Pali currently has no application-layer client authentication,
authorization policy, or security audit log. Recall feedback is local retrieval
telemetry, not an audit trail. Protect the endpoint, plaintext repository,
token, and remote using deployment controls appropriate to their sensitivity.
