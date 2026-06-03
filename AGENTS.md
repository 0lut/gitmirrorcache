# Agent Guidelines — gitmirrorcache

## Git argument sanitization

All `git-cache-git` methods that shell out to `git` **must** validate caller-supplied args. Unvalidated strings risk flag injection (`-` prefix) or NUL-byte truncation.

### Rules

1. **Validate early** — call `reject_*` helpers (`reject_ref_arg`, `reject_revision_arg`, `reject_config_key`, …) at the top of every public method forwarding input to git. Reject: empty strings, `-`-prefixed, `\0`-containing.
2. **Use `--`** to separate flags from positional args wherever git accepts it.
3. **Pick the narrowest validator** — `reject_config_key` for config (allows `=`), `reject_ref_arg` for refs (rejects `:`), `reject_revision_arg` for revisions (allows `:` for `HEAD:path`).
4. **Never pass unvalidated external input to git** — URLs, query params, request body fields included.

### New git wrapper checklist

1. Identify external-input args.
2. Choose/create appropriate `reject_*` validator.
3. Call validator before `self.run(…)`.
4. Add `--` where git supports it.
5. Test that `-`-prefixed input is rejected.

## Runtime safety

Panics must never happen in production code. OOM from unbounded allocations
must not happen. Resource exhaustion (processes, file descriptors, disk) must
be bounded.

### Mutex poisoning

**Never use `.expect()` or `.unwrap()` on `Mutex::lock()` in production code.**
A poisoned mutex means another thread panicked while holding the lock. Calling
`.expect()` converts that into a second panic, permanently bricking the
subsystem.

Correct pattern (already used in `git-cache-worker`):

```rust
let state = self.state.lock()
    .map_err(|_| GitCacheError::Internal("description of what poisoned".into()))?;
```

When the return type is not `Result` (e.g. `-> bool`), use:

```rust
let Ok(mut state) = self.state.lock() else {
    return <safe_default>;
};
```

`.expect()` / `.unwrap()` on locks is acceptable **only** in `#[cfg(test)]` code.

### Bounded allocations

1. **Never load an entire remote object into memory when only metadata is
   needed.** Use `ObjectStore::head()` to get size without downloading.
2. **Stream large blobs (bundles, pack files) through disk** — do not
   accumulate in a `Vec<u8>`. Use `ObjectStore::put_file()` for uploads
   from local files.
3. **Bound every output stream.** Any `AsyncRead` piped to an HTTP response
   must enforce a byte limit (see `ChildGuardStream`). Unbounded streams
   let a malicious or corrupted repo exhaust memory.
4. **`list_prefix` accepts `max_keys`** — pass a reasonable limit when the
   full listing is not required. Unbounded key listing can OOM on large
   namespaces.
5. **Git subprocess output is bounded** by `read_bounded()` with
   `max_git_output_bytes`. Never bypass this by reading stdout directly
   without a limit.

### Resource exhaustion

1. **Concurrent git processes are bounded** by a `tokio::sync::Semaphore`
   on the `Git` struct. Every `spawn()` must acquire a permit first.
   For streaming responses (`UploadPackProcess`), the permit is held until
   the process exits.
2. **Session directories must be cleaned up.** Sessions have a TTL but are
   only removed when cleanup runs. `SessionCleanupLoop` runs periodically
   to prevent unbounded inode/disk accumulation.
3. **Disk reservations clean up on drop** via `Reservation::Drop` /
   `AsyncReservation::release()`. Always prefer explicit `release()` over
   relying on drop, and never call `temp_path()` after `release()`.
4. **`kill_on_drop(true)`** is mandatory on all `tokio::process::Command`
   child processes to prevent zombie accumulation.

## Deployment operations

Use checked-in deployment scripts instead of ad-hoc AWS/SSM/Docker commands.
For the maintained ECS/EC2/EBS deployment, use:

```sh
AWS_REGION=us-west-2 ENVIRONMENT=dev-arm NAME_PREFIX=gitmirrorcache-arm scripts/aws/deploy-and-smoke.sh
```

If an ECS rollout is stuck because a prior-revision container is still holding
host port `8080`, inspect with `scripts/aws/ecs-host-diagnostics.sh`, then use
`scripts/aws/stop-stale-ecs-container.sh` with `ECS_STALE_CONTAINER_ID` and
`CONFIRM_STOP=true`. This script validates the ECS task family/container labels
before stopping anything.
