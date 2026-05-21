# Implementation Notes: Read-Through Git Remote

## Overview
Implementing the spec from `git-api-plan.md` (PR #13): a read-through Smart HTTP
remote that allows `git clone`/`git fetch` directly against the cache server
without needing a prior `/v1/materialize` call.

---

## Decisions not in the spec

### 1. Reusing the existing bare cache repo as the served repo
The spec says "Start with the existing bare cache repo as the served repo" and
only introduce a separate served bare repo if ref isolation becomes necessary.
We follow this guidance: the direct Git remote serves from the same
`cache_root/repos/{host}/{owner}/{repo}.git` bare repo that the materializer
already manages. Public `refs/heads/*` and `HEAD` are written into the same
repo. Internal `refs/cache/*` are hidden via `git config` on the repo.

### 2. Streaming upload-pack via Axum's `StreamBody`
The spec requires streaming pack output. The existing `upload_pack_stateless_rpc`
buffers stdout into a `Vec<u8>`. For the direct remote, we add a new
`upload_pack_stream` method on the `Git` wrapper that returns an async reader
(the child process stdout) instead of buffering. The API layer wraps this in
an Axum `Body::from_stream()`.  We keep the old buffered method for the session
endpoint for backward compatibility.

### 3. Buffered ref advertisement
`git upload-pack --advertise-refs` output is typically small (a few KB). We
reuse the existing buffered `advertise_refs()` helper and prepend the
`# service=git-upload-pack` header in-memory (a fixed 34-byte prefix).
We initially tried streaming but the child process lifetime issue (see T2)
made buffering the simpler and more correct approach for this small payload.

### 4. Config structure for `git_remote`
The spec suggests a `[git_remote]` TOML section. We add a `GitRemoteConfig`
struct with `enabled: bool` (default `false`). Branch ref check policy and
commit read-through behavior follow the spec defaults. The config is optional;
when absent, the direct remote is disabled and the router doesn't register
the `/git/{*repo_path}` route.

### 5. Upstream ref comparison uses `git ls-remote --heads --symref`
For comparing upstream refs before advertising, we use a single
`git ls-remote --heads --symref <upstream>` call. This gets all branch heads
and the symbolic HEAD in one round-trip. We parse both symrefs and branch SHAs
from the output.

### 6. Public ref sync strategy
After verifying upstream refs, we update the bare cache repo's public refs:
- `refs/heads/<branch>` → verified commit SHA
- `HEAD` → symref to default branch
We use `git update-ref` for branch heads and `git symbolic-ref HEAD` for the
default branch. This is done inside the lease to prevent races.

### 7. Concurrency: direct domain-layer synchronization
The spec says concurrent requests should wait for the same upstream work. For the
direct Git remote, we bypass the `UpdateCoordinator` and call the domain's
`ensure_repo_advertisable()` directly. This method does upstream ref comparison
and targeted fetch internally, which is simpler than routing through the
coordinator for the branch-comparison-then-fetch flow. The coordinator remains
in use for the existing `/v1/materialize` endpoint.

### 8. Want-line parsing
For `POST /git-upload-pack`, we parse `want <oid>` lines from the pkt-line
formatted request body. We check each wanted OID against local objects and
object-store manifests, hydrating as needed before invoking upload-pack.

### 9. `uploadpack.allowAnySHA1InWant=true`
We configure this on all served repos so that `git fetch origin <sha>` works.
The spec notes this should only be on "validated, allowlisted repos" — our
repos are already validated via `allowed_upstream_hosts`, so this is safe.

### 10. Integration tests against real public repos
The spec mentions tests with `git clone --branch main`, `git fetch <sha>`, etc.
We add integration tests that spin up the Axum server and run real git commands.
For high-commit repos, we looked for public repos with 200k+ commits.

Candidates:
- `astral-sh/uv` — already in the test suite
- `torvalds/linux` — ~1.2M+ commits, the canonical huge repo
- `chromium/chromium` — extremely large, but impractical for CI (very slow)
- `gcc-mirror/gcc` — ~300k+ commits
- `llvm/llvm-project` — ~500k+ commits

We use `torvalds/linux` and `llvm/llvm-project` as the high-commit test targets.
For CI practicality, we do shallow operations (ls-remote, single-branch fetch)
rather than full clones.

---

## Tradeoffs

### T1. No separate served repo
Using the same bare repo for internal cache refs and public refs is simpler but
means we need `hideRefs` config to prevent leaking `refs/cache/*`. If future
features need stronger isolation, we'll need to refactor to use alternates.

### T2. Buffered advertise-refs, streaming upload-pack
Initially we tried streaming both `advertise-refs` and `upload-pack` output.
However, child process lifetime management caused issues: the `UploadPackProcess`
struct holds the child with `kill_on_drop(true)`, and when it drops at the end of
the handler scope, the child is killed before the response stream is consumed by
the client. For `advertise-refs` (output is small — just ref lines) we switched
to the existing buffered `advertise_refs()` helper. For `upload-pack` (output can
be arbitrarily large pack data), we use a `ChildGuardStream` wrapper that holds
the child process handle alongside the `ReaderStream`, keeping the process alive
for the duration of the HTTP response body.

### T2b. Multi-threaded tokio runtime needed for git integration tests
`#[tokio::test]` uses a single-threaded runtime by default. Because the tests
run blocking `git` CLI commands via `std::process::Command::output()`, they block
the only tokio thread and starve the Axum server spawned on the same runtime.
We use `#[tokio::test(flavor = "multi_thread")]` and `spawn_blocking` for all
git CLI calls in integration tests.

### T3. Always-compare branch policy
The spec explicitly chose "branches always latest" over TTL-based staleness.
This means every branch clone/fetch does at least one `ls-remote` round-trip to
GitHub. For high-traffic scenarios, the single-flight dedup amortizes this.

### T4. Full `ls-remote --heads` rather than per-branch
We compare all branch heads in one `ls-remote` call rather than checking
individual branches. This is slightly more data transferred but avoids multiple
round-trips and lets us update all stale branches in one fetch pass.

---

## Things I changed from existing code

### C1. New `Git::upload_pack_stream` method
Added alongside the existing buffered `upload_pack_stateless_rpc`. Returns a
streaming reader + child process handle for the API to manage.

### C2. New `Git::ls_remote_heads` method
Returns parsed branch→SHA map from `git ls-remote --heads --symref`.

### C3. `GitRemoteConfig` added to `AppConfig`
Optional field with `#[serde(default)]`. Zero impact on existing configs.

### C4. Direct route added conditionally
The `/git/{*repo_path}` route is only registered when `git_remote.enabled = true`.
Existing deployments are unaffected.

---

# Implementation Notes: Incremental Generations & Compaction

## Overview
Implementing the plan from PR #23: generation publishing now creates delta
bundles when a repository has a previous generation head, and generation chains
can be compacted into a new full-bundle root generation.

## Decisions not in the spec

### 1. Keep old notes and append this section
PR #23's diff deleted the prior `implementation-notes.md`, but the request for
this implementation explicitly asked for a running notes file. I preserved the
previous notes and appended a new section instead of replacing the file.

### 2. Compaction API returns `CompactionReport`
The plan's contract summary listed `compact_generation_chain` as returning
`Option<GenerationId>`, while the detailed section introduced `CompactionReport`.
I used `Option<CompactionReport>` for the domain API so callers get the new
generation plus old depth, old generations, and approximate bytes reclaimed.

### 3. Dry-run compaction reserves a synthetic generation ID
`git-cache compact --dry-run` reports the generation ID that would be used if it
ran now. It does not write bundles or manifests. Running the real compaction
later will allocate a different UUIDv7.

### 4. Repointing current manifests only
Compaction rewrites canonical commit manifests, canonical ref manifests, and the
default-branch manifest when they point at a compacted generation. Historical
`ref-updates` observation manifests are left intact because they are append-only
audit/history records keyed by the old generation.

### 5. Commit list ordering during compaction
Hydration walks chains from head to root, but compacted manifests store commits
in root-to-head order for readability and deterministic tests.

## Tradeoffs

### T1. Approximate `bytes_reclaimed`
The object-store trait does not expose metadata, so compaction computes
`bytes_reclaimed` by reading old bundles and summing their byte lengths. This is
accurate for local/S3 object bytes but costs extra reads during compaction.

### T2. Head update after generation publish
The generation bundle and per-commit/ref manifests are published through the
existing atomic-ish `GenerationPublish` helper, then `RepoGenerationHead` is
written afterward. If the final head write fails, the generation still exists and
can be found by commit/ref manifests; the next publish treats the old head as the
delta base and may duplicate some objects rather than corrupting state.

### T3. Fallback resets the chain
When incremental bundle creation fails, the code deletes any partial bundle file,
creates a full bundle, sets `parent_generation: None`, and resets
`tip_commits` to the current commit. This favors safe recovery over preserving
the old chain when the local hot cache cannot prove it has the old tips.

## Things I changed from the plan

### C1. CLI `--all` enumerates generation-head manifests
There is no repository registry, so `git-cache compact --all` lists
`repos/*/manifests/generation-head.json` in the object store and compacts those
repos.

### C2. Inline compaction ignores "nothing to compact"
When `[compaction].inline = true`, publish calls compaction after writing the new
head and treats `Ok(None)` as a no-op. Errors still propagate because inline
compaction is part of the publish path when enabled.

### C3. `Git::run` already supports dynamic args
The git wrapper's `run` accepts any `IntoIterator<Item = impl AsRef<OsStr>>`, so
no separate `run_vec` helper was needed for `bundle_create_incremental`.
