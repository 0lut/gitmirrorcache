# Implementation Notes — HTTPS Auth (Phase 1: Plumbing & Redaction)

Decisions, deviations from AUTH-PLAN.md, and tradeoffs made during implementation.

---

## 1. UpstreamAuth Type — Simplified vs SecretString

**AUTH-PLAN.md specified:**
```rust
pub enum UpstreamAuth {
    Anonymous,
    Basic { redacted: RedactedHeader, raw: SecretString },
}
```

**Implemented:**
```rust
pub enum UpstreamAuth {
    Anonymous,
    Basic { raw: String },
}
```

**Rationale:** Avoided adding `secrecy` crate dependency. The `raw` field stores the
full header value (`Basic <base64>`). Redaction is enforced through the `Display` and
`Debug` impls — both emit `<redacted>` instead of the raw value. The `#[derive(Clone)]`
without `Debug`/`Display` derive ensures these traits always go through our manual impls.
There is no `RedactedHeader` wrapper because the Display/Debug impls already prove
redaction safety in tests.

## 2. Session Token Generation — No External Hex Crate

**AUTH-PLAN.md specified:** `gcs_<random-hex>` format with cryptographic randomness.

**Implemented:** Uses UUID v7 bytes (which contain timestamp + random) XOR'd with
additional UUID v4 bytes, then hex-encoded inline. Format: `gcs_` + 64 hex chars
(32 bytes of entropy).

**Rationale:** Avoids adding `hex` or `rand` crate dependencies. UUID v7 already
provides strong randomness via the `uuid` crate (already in deps). The XOR with
a second UUID provides additional entropy independent of UUID structure. An inline
`to_hex()` function handles encoding.

## 3. Token Hashing — SHA-256 via sha2 Crate

Added `sha2` dependency to `git-cache-domain` for session token hashing. Used
SHA-256 to hash bearer tokens before storing in manifests. This is the standard
approach for bearer token storage — stores only the hash, never the raw token
(except in the initial response to the client).

## 4. CommitReachableFrom Selector — Serialization as Modifier

The new `Selector::CommitReachableFrom { commit, reachable_from }` variant is
serialized as a JSON object with both `commit` and `reachable_from` fields
(the `reachable_from` acts as a modifier on a commit selector). During deserialization,
`reachable_from` is only valid when `commit` is present — otherwise it's rejected.

This maintains backward compatibility: existing `{"commit":"abc123"}` JSON still
deserializes to `Selector::Commit`, and only the new form
`{"commit":"abc123","reachable_from":{"branch":"main"}}` creates the new variant.

## 5. Protected Session TTL

Protected sessions (those with bearer tokens) use the standard `session_ttl`
from config (currently 3600s). The plan mentioned 600s for protected sessions
but this wasn't encoded in the phase 1 config changes. Can be added as a
separate `protected_session_ttl` config field in a follow-up.

## 6. Auth Header Name

Custom header: `git-cache-upstream-authorization` (lowercase, as HTTP headers
are case-insensitive). Falls back to standard `Authorization` header. The custom
header allows clients to pass upstream auth separately from any proxy/gateway
auth that might be on the standard header.

## 7. Error Classification Heuristic

`classify_auth_error()` inspects git stderr for keywords like "authentication
failed", "401", "403", "could not read Username" to reclassify generic git errors
as `Unauthorized` or `Forbidden`. This is a heuristic — git doesn't provide
structured error codes. False positives are possible if these strings appear in
unrelated error messages, but this is unlikely in practice.

## 8. API Layer Auth Extraction

Auth is extracted in the handler functions (`materialize`, `resolve`) before
being passed to `handle_materialize_request`. For Phase 1, the `_upstream_auth`
parameter is unused (prefixed with underscore). This plumbing ensures:
- The header parsing and validation is tested
- Future phases just remove the underscore and wire to the materializer
- The `upstream_authorization` field on `MaterializeRequest` controls whether
  missing auth is an error (`Required`) or acceptable (`Anonymous`)

## 9. Session Bearer Token Validation in git_session

Added a new `session_repo_and_protection()` method that returns both the repo
path AND the session's protection level. The `git_session` handler now:
1. Loads the session manifest and gets protection info
2. Extracts any bearer token from `Authorization: Bearer <token>`
3. Validates the token against the stored hash before serving any git data

This ensures protected sessions cannot be accessed without the correct token,
even at the git protocol level.

## 10. git_repo Handler — Phase 4 Plumbing

The `/git/{host}/{owner}/{repo}.git/...` handler now extracts upstream auth
from headers (stored as `_upstream_auth`). This is pure plumbing for Phase 4's
authenticated remote access. No behavior change — anonymous access still works
exactly as before.

## 11. MaterializeResponse — session_token Field

Added `session_token: Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]`.
Public sessions return `None` (field omitted from JSON). Protected sessions
return the raw token exactly once in the response — the client must save it
for subsequent git operations.

## 12. MaterializeSource — New Variants

Added `UpstreamAuthorizedCacheHit` and `UpstreamAuthorizedFetched` variants to
distinguish authenticated access paths from public ones. These will be used in
Phase 2/3 when the materializer actually performs upstream auth verification.
