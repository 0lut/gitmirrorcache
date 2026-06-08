# Chaos Test Checklist

Automated tests currently cover the key local chaos cases:

- cached exact commit materialization after upstream is removed
- branch selector failure when upstream is unavailable
- default branch selector resolution
- branch force-push updating the branch manifest while retaining the old commit manifest
- local bundle hydration after deleting the local hot repo
- disk reservation failure and LRU eviction behavior
- locked/protected repos skipped during eviction
- stale temp and reservation marker cleanup
- upload-pack advertisement without receive-pack
- explicit receive-pack request rejection

Useful manual drills:

```sh
cargo test --workspace
cargo test -p git-cache-api cached_exact_commit_survives_upstream_offline
cargo test -p git-cache-disk reserve_evicts_unlocked_lru_repo_until_it_fits
cargo test -p git-cache-worker dedupes_concurrent_updates_for_same_repo_and_ref
```

For a multi-worker deployment drill, run two API processes against the same object store and separate `cache_root` directories. Materialize a branch on one process, delete the second process's local cache, then materialize the exact commit on the second process. It should hydrate from object storage without contacting upstream.
