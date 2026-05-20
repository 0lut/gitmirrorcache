# Git Fetch Cache: Rust Implementation Handoff
**Read-only Git cache for GitHub fetch operations; object storage source of truth, local block storage as disposable hot cache**
Prepared: May 18, 2026
## 1. Background and problem context
Many CI, build, and agent workloads repeatedly run `git fetch` or `git clone` against the same GitHub repositories. When GitHub is rate-limited, slow, or temporarily unavailable, these workloads become unreliable even when the requested content has already been fetched before.
The cache should absorb repeat reads while preserving correctness. The core distinction is that commits are immutable while branches are mutable. A cached commit can be reused safely if its object closure is known-complete. A branch name such as `main` or `feature/foo` cannot be trusted from cache in strict mode because it may have moved upstream.
The system should be multi-cloud by design. AWS can be used for the first deployment, but the architecture should depend only on generic primitives: object storage, block storage, workers, and a small amount of lease/coordination logic.
## 2. Requirements and non-goals
### Requirements
- Support fetch/materialization for a specific commit SHA.
- Support fetch/materialization for a specific branch name, with strict mode verifying GitHub before serving.
- Support fetch/materialization for latest/default branch, with strict mode verifying GitHub before serving.
- Use cache whenever consistency permits it, especially for already-cached complete commits.
- Provide local development support using local fake upstreams and an S3-compatible object-store emulator.
- Manage disk allocation, reservations, temporary space, and eviction explicitly.
- Run multiple workers, ideally three or more, without relying on replicated block storage as source of truth.
- Support cron updates, read-through updates, and optional event-triggered updates as simple fetch hints.

### Non-goals
- No push support. git-receive-pack must not be advertised and push routes must be rejected.
- No Rust reimplementation of Git internals in v1.
- No dependency on one cloud vendor's proprietary-only service model. AWS services can be used behind portable abstractions.
- No guarantee that an uncached arbitrary commit SHA can be fetched if it is not reachable from an advertised upstream ref.

## 3. Consistency contract
| Request | Behavior | Works if GitHub is down? | Failure semantics |
|---|---|---|---|
| Exact commit | Use cache if the commit has a complete manifest and object closure. Contact GitHub only if the commit is unknown or incomplete. | Yes, if known-complete. | 503 if unknown and GitHub is unavailable. 404 only after upstream verification. |
| Specific branch | Strict mode always contacts GitHub to verify the branch head, then serves the resolved commit through a pinned session ref. | No in strict mode. Optional cached mode can serve only within an explicit staleness bound. | 503 if GitHub is unavailable. 404 only if branch absence is verified upstream. |
| Latest/default branch | Resolve default branch from upstream, then behave like strict branch. | No in strict mode. Optional cached mode can serve only within an explicit staleness bound. | 503 if GitHub is unavailable. |
| Cached branch/latest | Optional escape hatch for callers that accept bounded staleness. | Yes, if last verified observation is within max_staleness. | Return stale/unavailable according to caller policy if too old. |
| Push | Unsupported. | Not applicable. | 405 or equivalent rejection; never advertise receive-pack. |

## 4. High-level architecture
```text
Clients
  |
  |  POST /v1/materialize, POST /v1/resolve, or Git smart HTTP
  v
API / Git Frontend / Load Balancer
  |
  +--> Resolver and consistency policy
  +--> Session manager for pinned synthetic refs
  +--> Worker fleet, each with local block-cache repo storage
          |
          +--> Local bare repos on block storage
          +--> Disk manager: quotas, reservations, temp space, eviction
          +--> Git process wrapper: fetch, fsck, bundle, upload-pack
          |
          v
      Object storage: bundles, generations, manifests, sessions, leases
          |
          v
      GitHub upstream, contacted only when required by contract
```

### Components
| Component | Responsibility |
|---|---|
| API service | Receives resolve/materialize requests, applies consistency policy, creates pinned sessions, exposes health and admin endpoints. |
| Git smart HTTP frontend | Serves git-upload-pack for session URLs and optionally cached mirror URLs. Push endpoints are rejected. |
| Worker | Owns a local block-cache directory, runs Git commands, hydrates repos from object storage, and publishes new generations. |
| Object storage | Durable source of truth for bundles, manifests, sessions, leases, and update metadata. |
| Disk manager | Tracks cache usage, reserves space before Git operations, evicts LRU repos, and protects minimum free space. |
| Updater | Performs cron, read-through, and event-triggered upstream fetches. Events are hints; successful Git fetches are the source of truth. |
| Lease manager | Uses conditional object-store writes to avoid stampedes and concurrent conflicting publishes. |

## 5. Why materialization sessions are needed
A normal Git smart HTTP fetch starts by asking the server to advertise refs. That does not give the service a clean, explicit point to enforce the rule that a branch request must verify GitHub first. Therefore strict branch/latest requests should go through a control-plane materialization API.

The materialize endpoint verifies the requested selector, resolves it to a commit, ensures the commit is available, writes a short-lived session manifest, and returns a session Git URL plus a synthetic ref. The client then fetches that synthetic ref from the cache. This makes the strict verification boundary explicit and auditable.

## 6. API surface
### Materialize
```http
POST /v1/materialize
```
Request examples:
```json
{ "repo": "github.com/org/repo", "selector": { "branch": "main" }, "mode": "strict" }
{ "repo": "github.com/org/repo", "selector": { "default_branch": true }, "mode": "strict" }
{ "repo": "github.com/org/repo", "selector": { "commit": "abc123..." }, "mode": "strict" }
```
Response:
```json
{
  "repo": "github.com/org/repo",
  "commit": "abc123...",
  "source": "github_verified | cache_verified",
  "verified_at": "2026-05-18T19:00:00Z",
  "git_url": "https://git-cache.internal/git/session/01HX.../github.com/org/repo.git",
  "ref": "refs/cache/sessions/01HX...",
  "expires_at": "2026-05-18T20:00:00Z"
}
```
### HTTP status semantics
| Status | Meaning |
|---|---|
| 200 | Materialized or resolved successfully. |
| 202 | Optional: another worker is already fetching and caller chose async/wait behavior. |
| 404 | Ref/commit was verified absent upstream. |
| 503 | GitHub is required by the consistency contract but unavailable or rate-limited. |
| 507 | Worker cannot reserve enough local disk and eviction cannot free sufficient space. |
| 405 | Push or unsupported Git operation attempted. |

## 7. Storage model
### Object storage as source of truth
```text
s3://git-cache/
  repos/
    github.com/org/repo/
      manifests/
        repo.json
        refs/heads/main.json
        refs/heads/feature%2Ffoo.json
        commits/ab/abc123....json
        sessions/01HX....json
      generations/
        000001/manifest.json
        000001/base.bundle
        000002/manifest.json
        000002/incremental.bundle
      leases/update.json
```
A generation is published only after the worker has fetched from upstream, verified local repository connectivity, created a bundle, uploaded the bundle, and written the generation manifest. Ref and commit manifests are updated after the generation is durable.

### Local block storage as disposable cache
```text
/cache/
  repos/github.com/org/repo.git
  tmp/hydrate-01HX...
  reservations/01HX....json
  index/repo-index.json
```

## 8. Rust implementation plan
Rust should implement the control plane and safety boundaries, not Git's object/protocol internals. The service invokes the `git` binary through a hardened wrapper. The only Git service to advertise is `git-upload-pack`; `git-receive-pack` is disabled and rejected.

| Crate | Responsibility |
|---|---|
| git-cache-core | Types, config, errors, RepoKey parsing, selectors, manifests, status models. |
| git-cache-git | Safe wrapper around git CLI: fetch, rev-parse, fsck, bundle, update-ref, upload-pack. |
| git-cache-objectstore | Portable ObjectStore trait, S3-compatible adapter, local filesystem adapter for tests. |
| git-cache-disk | Quota accounting, reservations, eviction, temp directories, repo size scanning. |
| git-cache-api | Axum HTTP API, materialize/resolve endpoints, Git smart HTTP session endpoints. |
| git-cache-worker | Cron updater, read-through update path, event/task processing. |
| git-cache-cli | Admin commands: inspect, warm, repair, prune, disk-status. |

## 9. Disk allocation and local cache management
Disk management is first-class. Every Git operation reserves space before it writes temporary packs or local repo data. If quota would be exceeded, the service evicts unlocked LRU repos; if eviction cannot free enough, the request returns 507.

Sizing formula:
```text
worker_volume_size =
  hot_repo_bytes
  + concurrent_git_ops * p95_repo_bytes * tmp_multiplier
  + min_free_bytes
  + safety_margin
```

## 10. Local development setup
Use MinIO or another S3-compatible emulator for object storage, local bare repositories as fake GitHub upstreams, and an app-level disk quota for disk pressure tests.

## 11. Update flows
| Flow | Steps |
|---|---|
| Strict branch | Acquire repo lease, fetch requested branch from GitHub, resolve tip, verify connectivity, publish generation, update ref/commit manifests, create session. |
| Strict default branch | Resolve upstream HEAD/default branch, then follow strict branch flow. |
| Exact commit | If manifest complete, create session from cache. If unknown, try upstream; fail 503 if unavailable; publish if fetched. |
| Cron update | Periodically fetch configured repos/refs, verify, publish generations, and refresh manifests. |
| Read-through | On strict request, fetch inline behind a lease. Concurrent callers wait, reuse result, or get policy-specific response. |
| Events | Treat webhooks/events as hints only. The successful Git fetch is the authoritative update. |

## 12. Fault tolerance and scaling
| Failure or event | Expected behavior |
|---|---|
| Worker dies | Another worker can serve after reading manifests and hydrating local cache from object storage. |
| Worker disk lost | Treat as cold cache. Rehydrate from object storage. No durable data is lost. |
| GitHub down | Known-complete cached commits continue to work. Strict branch/latest and uncached commit requests fail. |
| Object storage down | Hot local repos may serve existing sessions only if policy allows; publishing and cold hydration stop. |
| Local repo corruption | Delete local repo, rehydrate from object storage, verify with fsck. |
| Concurrent updates | Per-repo leases prevent stampedes. Manifest updates use conditional writes. |
| Force push | Branch manifest moves to the new commit after upstream verification. Old cached commits may remain fetchable by commit/session until retention expiry. |

## 13. Security and safety requirements
- Validate RepoKey and branch/ref inputs; never allow path traversal or arbitrary filesystem paths.
- Allowlist upstream hosts and credential sources.
- Never call the shell; invoke git with explicit argument arrays.
- Set GIT_TERMINAL_PROMPT=0 and use a controlled HOME/Git config environment.
- Timeout every Git process and bound stdout/stderr capture.
- Reject and do not advertise git-receive-pack.
- Run workers as non-root and avoid executing hooks.
- Do not store secrets in manifests or logs.

## 14. Milestone plan
| Milestone | Theme | Deliverables |
|---|---|---|
| M1 | Skeleton | Cargo workspace, config loader, axum server, healthz, structured logging. |
| M2 | Git wrapper | init-bare, fetch-branch, rev-parse, fsck, bundle, fetch-from-bundle, upload-pack wrapper. |
| M3 | Object store | ObjectStore trait, S3-compatible adapter, local adapter, manifests, bundles, conditional writes. |
| M4 | Disk manager | Quota accounting, reservations, eviction, locks, temp cleanup, disk-status admin command. |
| M5 | Exact commit materialization | Commit manifest lookup, cache hydrate, session creation, cached commit works with upstream offline. |
| M6 | Strict branch/default | Upstream verification, branch/default resolution, 503 when upstream unavailable. |
| M7 | Git smart HTTP | Session-aware info/refs and upload-pack endpoints; push rejection. |
| M8 | Updaters and leases | Cron, read-through, event hints, per-repo leases, concurrency dedupe. |
| M9 | Production hardening | Metrics, rate limits, credential handling, compaction, chaos tests, multi-worker deployment. |

## 15. Acceptance criteria
- A known-complete cached commit can be fetched successfully while GitHub/upstream is unavailable.
- A strict branch request contacts upstream and returns 503 when upstream is unavailable.
- A strict default branch request resolves the upstream default branch and behaves like strict branch.
- A branch force-push updates the branch manifest after verification without deleting the old cached commit immediately.
- Deleting a worker's local cache does not lose durable state; the worker can rehydrate from object storage.
- Disk quota tests either evict LRU repos or return 507 without corrupting local repos.
- Concurrent requests for the same repo produce one upstream fetch behind a lease, not a thundering herd.
- Push attempts are rejected and receive-pack is never advertised.
- Local development can run without real AWS or GitHub by using local fake upstreams and S3-compatible object storage.

## 16. Open decisions
| Decision | Notes |
|---|---|
| Session TTL | How long should pinned session refs stay valid? Initial proposal: 1 hour, configurable. |
| Retention | How long should old commits/generations remain in object storage? Initial proposal: 30-90 days depending repo class. |
| Bundle strategy | Start with full bundles for simplicity, then add incremental bundles and compaction. |
| Unknown commit fetch | Whether callers must provide a branch/ref hint for uncached arbitrary SHAs not reachable from advertised refs. |
| Cached branch mode | Whether to expose bounded-staleness branch/latest as an official API or keep it internal. |
| Queue implementation | For portability, object-store tasks or NATS/Postgres can be used; avoid hard dependency on AWS-only queue semantics in core design. |

## 17. Implementation checklist
- Create Rust workspace and crate boundaries.
- Implement config schema, including local dev and production examples.
- Implement hardened Git command wrapper and integration tests against local bare repos.
- Implement S3-compatible object store and local filesystem object store.
- Implement manifest read/write, generation publish protocol, and conditional leases.
- Implement disk reservations, LRU eviction, and repo cache indexing.
- Implement materialize exact commit, then strict branch, then default branch.
- Implement session Git smart HTTP endpoints using git-upload-pack.
- Add cron/read-through update flows and concurrency dedupe.
- Add observability, security hardening, and chaos tests.
