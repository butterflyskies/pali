# ADR-0018: Dedicated GitHub App for release-please

## Status
Superseded by [ADR-0045](0045-forgejo-tag-authority-github-release-executor.md)

## Context
Release-please creates and maintains a release PR on every push to main. When it uses
the default `GITHUB_TOKEN`, PRs it creates do not trigger `pull_request` workflows — this
is GitHub's anti-cascade safety rule. With branch protection requiring CI checks to pass,
release-please PRs were permanently blocked (zero checks, can't merge).

The fix is to use a token from a different identity so that PR creation triggers workflows.
Three options were considered:

1. **Fine-grained PAT** — simple, but tied to a personal account with manual expiry rotation.
2. **Reuse the existing `claude` GitHub App** — already installed org-wide with sufficient
   permissions, but overloaded identity (release PRs would show as authored by claude[bot]).
3. **Dedicated GitHub App** — minimal permissions, single purpose, clear provenance.

## Decision
Create a dedicated GitHub App (`butterflyskies-release-manager-bot`, app ID 3144639) with only `contents:
write` and `pull_requests: write` permissions. Store the App ID and private key as org-level
secrets (`RELEASE_BOT_APP_ID`, `RELEASE_BOT_PRIVATE_KEY`). Use `actions/create-github-app-token`
in the release workflow to generate a scoped, short-lived token for release-please.

## Consequences
- Release-please PRs trigger CI workflows and can pass required status checks
- Clear provenance: release PRs are authored by `butterflyskies-release-manager-bot[bot]`,
  distinct from human and AI contributors
- Org-level secrets mean any repo in the org can reuse the same app
- No long-lived tokens — app installation tokens auto-expire after 1 hour
- Private key backed up in 1Password for disaster recovery
- One more GitHub App to maintain (minimal — no webhook, no rotation needed)
