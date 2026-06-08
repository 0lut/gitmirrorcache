# Revised Auth Optimization Plan

This is the current plan for PR #45 after the LLVM performance investigation.
Earlier versions explored GitHub REST probes, public/private classification, and
per-object upstream proof. Those ideas are deliberately superseded here.

## Goals

- Keep auth-free public usage available.
- Keep request-scoped upstream credentials available for private repos.
- Keep service auth out of scope for this PR.
- Avoid authenticated/unauthed materializer method forks.
- Preserve the current `main` speed profile for hot materialize and hot direct
  clones.
- Preserve current `main` read-through behavior for direct Git clones, including
  cold clones that have not been pre-materialized.

## Core Rule

After repo access is checked, downstream code should not care whether auth was
empty or credentialed. It should receive an access context and proceed through
the same materialize, resolve, or direct-serving code.

For this PR, repo access is the authorization boundary. If a caller can read the
upstream repo with the selected auth, the caller may request cached history for
that repo. Deployments that need stricter isolation for rewritten or hidden
history should use separate upstream repos or add a future current-reachability
policy.

## Auth Selection

No token:

```text
use UpstreamAuth::Anonymous
prove repo/ref access through upstream Git
return public sessions/refs where applicable
```

Token present:

```text
use the supplied UpstreamAuth
prove repo/ref access through upstream Git
return protected sessions where applicable
```

Do not downshift token-present requests to anonymous mode. Do not use GitHub REST
to classify a repo as public/private in this PR. A bad supplied token should fail
visibly instead of silently falling back to public access.

## API Shape

The API layer:

- parses upstream credentials from `Git-Cache-Upstream-Authorization` for
  `/v1/*`;
- parses `Authorization: Basic ...` as upstream credentials for direct Git;
- rate-limits before upstream work;
- checks `UpstreamAuthorizationMode::Required` has credentials;
- creates `Materializer::using_upstream_auth(&auth)`;
- calls the unified materializer directly for `/v1/materialize` and
  `/v1/resolve`;
- does not perform provider-specific repo authorization.

The domain layer proves access by running the normal upstream Git operations
with the selected auth.

## Materialize Shape

Materialize builds one `MaterializePlan`:

```text
request + selected upstream auth
  -> prove/resolve target repo/ref/commit
  -> RepoAccessContext + target commit/ref
  -> materialize target
  -> create public or protected session from access context
```

Branch and default branch:

- resolve the current upstream tip with selected auth;
- if commit and tree are already locally ready, update refs/manifests and return;
- otherwise fetch the target branch with selected auth;
- verify the fetched SHA still matches the resolved upstream tip;
- publish the generation and create the session.

Exact commit:

- first prove repo access with a lightweight upstream default-branch check;
- use complete commit manifests or known local generation ancestry when possible;
- fetch upstream refs with selected auth only when needed to find/build the
  commit.

Short commit:

- fetch upstream refs with selected auth;
- resolve locally;
- publish/index the resolved commit.

## Resolve Shape

`/v1/resolve` uses the same selector policy as materialize but returns only
`ResolveResponse`: repo, selector, commit, source, `cache_available`, and
`authorized_at`. It never creates a session and no longer has a separate
anonymous materialize-compatible response path.

## Direct Git Shape

Direct Git GET:

- validates the repo;
- parses upstream auth;
- fetches upstream refs with selected auth as the repo-access proof;
- synthesizes the Smart HTTP ref advertisement from the current upstream refs;
- stores a short-lived proof handoff keyed by repo and exact auth fingerprint.

Direct Git POST:

- validates the repo;
- parses upstream auth;
- uses the matching GET proof handoff when present;
- otherwise reruns the same lightweight upstream ref fetch;
- calls one domain `handle_upload_pack` path.

Domain upload-pack:

- parses wants;
- serves locally ready wanted commits immediately;
- hydrates complete commit manifests when available;
- fetches missing wanted commits from upstream using the same request auth;
- requires `commit_ready_for_serving` before exposing a fetched or hydrated
  commit;
- publishes a generation for newly imported commits;
- exposes served commits through hidden refs;
- configures the served repo;
- spawns `git upload-pack`.

The direct Git POST path is still one path, not an authenticated/unauthed fork.
Object checks are availability checks after repo access, not a second
authorization phase. A future stricter mode can add current-reachability proof,
but the default PR behavior follows current `main`: repository access is
sufficient and direct clone can read through.

## Provider Plan

This PR uses provider-neutral Git transport for GitHub, GitLab, Bitbucket, and
other allowed HTTPS hosts. The future provider model can introduce explicit
origin types such as `GitHubOrigin`, `GitLabOrigin`, `BitbucketOrigin`, and
`PrivateGitServerOrigin`, but only after we have measured a need.

## Security Caveats

- Session bearer tokens are never stored raw; only token hashes are persisted.
- Request auth must stay out of argv, logs, local git config, object-store keys,
  and manifests.
- The shared repo model means repo-level access can expose cached history for the
  repo, even if that history is no longer reachable from public refs. This is an
  accepted tradeoff for this PR and is documented in code comments.
- Future hardening options: ephemeral direct-serving repos, optional
  current-reachability policy, provider-specific access probes, and service auth.

## Performance Expectations

- Hot branch/default materialize should be close to current `main`: one upstream
  ref resolution plus local readiness/session work.
- Hot direct clone after a matching GET should avoid the second upstream ref
  proof and stay near current `main`.
- Cold direct clone should work without pre-materialization by read-through
  fetching the wanted commit, then publishing the generation.
- Expensive rebuilds must be no worse than current `main` for the same cache
  state; auth should add only the repo-access proof.
- New generation publication should verify the just-created bundle from the
  local repo when parents are already verified, then publish verified metadata
  immediately.
- Pending generation verification should use the same local-repo proof and only
  download the pending bundle, rather than re-indexing the whole parent chain.

The worker `UpdateCoordinator` remains useful for cron/event-driven warming, but
the HTTP API must not pre-run it before the unified materializer. Doing so can
turn a hot local branch request into pending-generation verification or publish
work, which regresses the `main` hot-path speed profile.

## Tests

Current local verification:

- `cargo test -p git-cache-domain materializer::tests::direct_git_tests -- --nocapture`
- `cargo test -p git-cache-api --lib -- --nocapture`
- `cargo test -p git-cache-domain materializer::tests::selector_tests -- --nocapture`
- `cargo test -p git-cache-domain --lib -- --nocapture`
- `cargo test --workspace -- --nocapture`

The workspace suite passes locally. The full command needs an unsandboxed test
run in this environment because several integration tests bind local TCP ports.
