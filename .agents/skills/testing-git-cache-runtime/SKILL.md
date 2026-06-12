---
name: testing-git-cache-runtime
description: Test gitmirrorcache runtime cache flows end-to-end. Use when verifying materialization, generation manifests, compaction, or cold-cache hydration behavior.
---

# Git Cache Runtime Testing

Requirements: local Rust/Git toolchain only; no secrets; no live-infrastructure
access.

This local-only runbook covers runtime and cache-flow testing. Follow the shared
repository rules in [AGENTS.md](../../../AGENTS.md); privileged AWS deployment
and recovery operations live in
[gitmirrorcache-deploy](../../../.devin/skills/gitmirrorcache-deploy/SKILL.md).

## Secrets Needed

None for local runtime testing. The flow uses a local bare upstream repository, a local object store, and local cache directories.

## Local build commands

From the repo root, build the runtime binaries used by this flow:

```bash
cargo build -p git-cache-cli -p git-cache-api
```

The CLI binary is `target/debug/git-cache`; the API binary is `target/debug/git-cache-api`.

## Minimal runtime environment

Use an isolated test root under the home directory, not `/tmp`, so artifacts are not unexpectedly cleaned during a long session.

Create a local config file with:

- `cache_root` pointing at the isolated cache directory
- `upstream_root` pointing at a local `upstreams` directory
- `[object_store] kind = "local"` with `root` pointing at the isolated object-store directory
- `allowed_upstream_hosts = ["github.com"]`
- `[disk] min_free_bytes = 0` for small local test fixtures
- `[compaction] chain_depth_threshold = 2` when you need a three-pack generation head to compact quickly (the threshold counts packs referenced by the head generation manifest)
- `[compaction] inline = false` unless inline compaction itself is the feature under test

Set `GIT_CACHE_CONFIG=/path/to/config.toml` for both CLI and API commands.

## Primary end-to-end compaction flow

1. Create a local bare upstream at `upstreams/github.com/acme/repo.git` and a working repo with user name/email configured.
2. Create commits `A`, `B`, and `C` on branch `main`; after each commit, force-push `main` to the bare upstream and run:

   ```bash
   target/debug/git-cache warm github.com/acme/repo main
   ```

3. If testing branch-ref repointing for a branch literally named `default`, do not use `git-cache warm github.com/acme/repo default`: the CLI reserves selector text `default` for the default-branch selector. Instead start `target/debug/git-cache-api` with the same `GIT_CACHE_CONFIG` and POST:

   ```json
   {"repo":"github.com/acme/repo","selector":{"branch":"default"}}
   ```

   to `http://<bind_addr>/v1/materialize`.

4. Inspect object-store JSON before compaction. For `github.com/acme/repo`, useful paths are:

   - `objects/repos/github.com/acme/repo/generations/<generation>/manifest.json`
   - `objects/repos/github.com/acme/repo/packs/pack-<sha256>.pack` (listed in each generation manifest's `packs` array)
   - `objects/repos/github.com/acme/repo/manifests/generation-head.json`
   - `objects/repos/github.com/acme/repo/manifests/refs/heads/main.json`
   - `objects/repos/github.com/acme/repo/manifests/refs/heads/default.json`

5. Run dry-run compaction first:

   ```bash
   target/debug/git-cache compact --repo github.com/acme/repo --dry-run
   ```

   Assert the report has `old_pack_count: 3` and the generation head still points to the pre-compaction head.

6. Run real compaction:

   ```bash
   target/debug/git-cache compact --repo github.com/acme/repo
   ```

   Assert the report has `old_pack_count: 3`, three `old_generations`, a non-empty `new_generation`, and `bytes_reclaimed > 0`.

7. Assert each old generation from the report no longer has a `manifest.json`, and that pack keys referenced only by old generations were deleted from `packs/`.

8. Assert the new generation manifest has a single entry in `packs`, a non-null `verified_at`, and contains exactly commits `[A, B, C]`.

9. Assert branch ref manifests, including `refs/heads/default` when applicable, point to the new compacted generation.

10. Delete only the local cached bare repo directory, then warm exact commit `C`:

    ```bash
    target/debug/git-cache warm github.com/acme/repo <commit-c-sha>
    ```

    Assert the response has `source: cache_verified`, then run `git cat-file -e <sha>^{commit}` for commits `A`, `B`, and `C` inside the rehydrated cached bare repo.

## Reporting tips

This feature is backend/CLI-only, so do not record the browser. Capture command output and summarize each assertion as passed/failed/untested. If testing an open PR, post one collapsed PR comment with the runtime assertions and a short evidence excerpt.
