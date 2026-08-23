# ADR-0044: Federated straddled reads preserve store custody

## Status
Accepted

Design provenance: Lacuna collective design discussion, 2026-08-22. ADR-0043
is reserved by an active evaluation-contract worktree, so this decision uses
the next available number.

## Context

One logical memory can have deliberately different fragments in separate Pali
instances: a friends-common fragment, a Lacuna-internal fragment, and a
construct-private fragment. Today a caller discovers those fragments by
probing every store by name. That preserves custody, but it is repetitive,
serial in many clients, and unable to distinguish "not found" from "the store
could not be reached" without client-specific conventions.

Treating the stores as one database would erase the access-control boundary
that makes the split useful. Giving one Pali a service credential for all of
its siblings would instead make it a confused deputy: a caller could acquire
the service's authority merely by asking for a straddled read. Pali currently
has outbound Git credentials, but no inbound principal or IAM subsystem that
could safely manufacture delegated authority.

## Decision

Pali adds an opt-in federated `read` mode. A local store may name itself and
configure trusted sibling MCP endpoints. When `straddle: true` is requested:

1. The local read and all sibling reads run concurrently under one bounded
   timeout. A sibling timeout or outage never blocks a successful local read.
2. Every result is keyed by store and carries an explicit status and
   provenance. `not_found` means the sibling answered authoritatively;
   `unreachable` means it did not. Those states are never collapsed to `null`.
3. The inbound bearer credential is treated as an opaque caller delegation.
   It is copied only to administrator-configured sibling endpoints, is never
   persisted or logged, and is never replaced by a Pali service credential.
   Each sibling remains responsible for authorizing that credential.
4. If no caller credential is available, the local result is still returned
   and every sibling is marked `identity_unavailable`; Pali does not contact
   the siblings. This is fail-closed federation with graceful local service.
5. Sibling calls always use ordinary, non-straddled `read`, preventing
   recursive federation and topology loops.
6. Omitting `straddle` preserves the existing read response byte shape.

Configuration is deliberately topology-only: store ids, sibling URLs, and a
timeout. Tokens and sibling credentials are forbidden in configuration.

## Response contract

The additive straddled response has this shape:

```json
{
  "name": "person-cammy",
  "scope": "global",
  "stores": {
    "personal": { "status": "found", "memory": { "content": "..." } },
    "fcc": { "status": "not_found" },
    "lcc": { "status": "unreachable" }
  },
  "degraded": true
}
```

Operational error details are intentionally coarse. Responses do not disclose
tokens, sibling URLs, transport internals, or private topology beyond the
configured store ids already visible to the caller.

## Consequences

- Store custody and authorization remain independent while reads gain one
  logical address space.
- Federation can operate before Pali grows a full IAM/ABAC system, but only by
  propagating authority already presented by the caller.
- Bearer audience mismatches surface as an unavailable/denied sibling rather
  than triggering token exchange. Token exchange and audience-aware delegation
  belong to the future IAM layer.
- `recall --straddle` can later reuse the same transport and provenance model.
- `remember --store` is deferred until authenticated caller identity and a
  directional existence-metadata policy are enforceable. In particular, a
  shared store must never reveal that a private sibling contains the same
  name.

## Alternatives rejected

- **Unified physical store:** destroys independent custody and repository
  access control.
- **Service-level sibling tokens:** creates a confused deputy and violates
  least authority.
- **Unauthenticated sibling reads:** silently relies on network placement as
  disclosure policy.
- **Hard-fail the whole read on one sibling outage:** makes boot availability
  the intersection of every store's availability.
- **Treat outage as absence:** corrupts the epistemic distinction between
  "nothing there" and "could not ask."
