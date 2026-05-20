# Incremental Generations & Compaction — Implementation Plan

## Problem

Every call to `publish_generation` creates a **full bundle** (`git bundle create --all`)
and sets `parent_generation: None`.  For large repos this means:

- Multi-GB S3 writes on every branch update.
- Slow publish (minutes for repos like torvalds/linux).
- Wasted storage (N full copies of nearly-identical object sets).

The `GenerationManifest.parent_generation` field and the `hydrate_generation` chain-walk
already exist but are never used.

## Goal

Turn the generation system into an incremental one:

1. **Delta bundles (P0)** — each new generation contains only objects added since the
   previous generation, linked via `parent_generation`.
2. **Compaction (P2)** — periodically merge a chain of delta bundles into a single full
   bundle to bound hydration cost.

---

## Work Streams (parallelizable)

The plan is split into four independent work streams that share well-defined
interfaces.  Streams **A**, **B**, **C** can execute in parallel once the
contract types from stream **A-types** are merged.

```
A-types ──┬── Stream A (delta publish)
           ├── Stream B (compaction)
           └── Stream C (git-cache-git bundle methods)
```

After A/B/C merge, **Stream D** (integration tests) validates the full flow.

---

## A-types: Shared Contracts (merge first)

New types and manifest helpers that the other streams depend on.

### A-types.1 — `RepoGenerationHead` manifest

A small JSON document that tracks the latest generation for a repository.
Stored at `repos/{repo}/manifests/generation-head.json`.

```rust
// crates/git-cache-core/src/manifest.rs

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGenerationHead {
    pub repo: RepoKey,
    pub generation: GenerationId,
    /// Tip commits reachable in this generation (union of all commits
    /// in the chain).  Used as exclusion set for delta bundles.
    pub tip_commits: Vec<CommitSha>,
    pub updated_at: DateTime<Utc>,
}
```

### A-types.2 — Object-store helpers

```rust
// crates/git-cache-objectstore/src/manifests.rs

pub fn repo_generation_head_key(repo: &RepoKey) -> String {
    format!("repos/{repo}/manifests/generation-head.json")
}

pub async fn read_repo_generation_head<S: ObjectStore + ?Sized>(
    store: &S, repo: &RepoKey,
) -> Result<Option<RepoGenerationHead>> { ... }

pub async fn write_repo_generation_head<S: ObjectStore + ?Sized>(
    store: &S, head: &RepoGenerationHead,
) -> Result<()> { ... }
```

### A-types.3 — `CompactionConfig`

```rust
// crates/git-cache-core/src/config.rs

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Compact when the generation chain exceeds this depth.
    #[serde(default = "default_compaction_threshold")]
    pub chain_depth_threshold: u32,

    /// If true, run compaction inline after publish.  If false,
    /// compaction only runs via the CLI / cron.
    #[serde(default)]
    pub inline: bool,
}

fn default_compaction_threshold() -> u32 { 10 }

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            chain_depth_threshold: 10,
            inline: false,
        }
    }
}
```

Add to `AppConfig`:

```rust
#[serde(default)]
pub compaction: CompactionConfig,
```

---

## Stream A — Delta Publish (`git-cache-domain`)

Changes `Materializer::publish_generation` to produce delta bundles when a
previous generation exists.

### A.1 — Look up previous generation head

At the top of `publish_generation`, read `RepoGenerationHead` for the repo.
If present, use its `tip_commits` as the exclusion set.

### A.2 — Create a delta bundle

Instead of `bundle_create_all`, call a new git wrapper method:

```rust
// call site in materializer.rs
if !prev_tips.is_empty() {
    self.state.git
        .bundle_create_incremental(repo_dir, &bundle_path, &prev_tips)
        .await?
} else {
    self.state.git
        .bundle_create_all(repo_dir, &bundle_path)
        .await?
}
```

### A.3 — Set `parent_generation`

```rust
let generation_manifest = GenerationManifest {
    repo: repo.clone(),
    generation,
    bundle_key,
    parent_generation: prev_head.map(|h| h.generation),  // was: None
    created_at: now,
    commits: vec![commit.clone()],
};
```

### A.4 — Update `RepoGenerationHead`

After publishing, write the updated head:

```rust
let mut tip_commits = prev_head
    .map(|h| h.tip_commits)
    .unwrap_or_default();
tip_commits.push(commit.clone());

let head = RepoGenerationHead {
    repo: repo.clone(),
    generation,
    tip_commits,
    updated_at: now,
};
write_repo_generation_head(&*self.state.store, &head).await?;
```

### A.5 — Fallback to full bundle on delta failure

Delta bundle creation can fail if the local repo doesn't contain the
previous tips (e.g., after hot-cache eviction + re-clone).  Catch the
error and fall back to `bundle_create_all` with `parent_generation: None`.

```rust
match self.state.git
    .bundle_create_incremental(repo_dir, &bundle_path, &prev_tips)
    .await
{
    Ok(_) => { /* delta succeeded */ }
    Err(err) => {
        warn!(%repo, %err, "delta bundle failed, falling back to full bundle");
        self.state.git
            .bundle_create_all(repo_dir, &bundle_path)
            .await?;
        // Reset: this becomes a new root generation
        parent_generation = None;
        tip_commits = vec![commit.clone()];
    }
}
```

---

## Stream B — Compaction (`git-cache-domain`)

A new method on `Materializer` that merges a chain of delta generations
into a single full generation.

### B.1 — `Materializer::compact_generation_chain`

```rust
/// Compact the generation chain for a repo.  If the chain from the
/// current head back to a root exceeds `chain_depth_threshold`,
/// replace it with a single full-bundle generation.
///
/// Returns the new `GenerationId` if compaction occurred, or `None`
/// if the chain was already short enough.
pub async fn compact_generation_chain(
    &self,
    repo: &RepoKey,
) -> CoreResult<Option<GenerationId>> { ... }
```

Algorithm:
1. Read `RepoGenerationHead`.
2. Walk `parent_generation` chain, collecting all `GenerationManifest`s.
3. If chain length ≤ `compaction.chain_depth_threshold`, return `None`.
4. Hydrate the full chain (ensure all objects are local).
5. `git bundle create --all` → full bundle.
6. Publish a new `GenerationManifest` with `parent_generation: None`.
7. Re-point all `CommitManifest`s and `RefManifest`s that referenced any
   generation in the old chain to the new compacted generation.
8. Update `RepoGenerationHead` with the new generation and fresh `tip_commits`.
9. Delete old generation bundles and manifests from the object store.
10. Return `Some(new_generation)`.

### B.2 — `CompactionReport`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionReport {
    pub repo: RepoKey,
    pub old_chain_depth: usize,
    pub old_generations: Vec<GenerationId>,
    pub new_generation: GenerationId,
    pub bytes_reclaimed: u64,
}
```

### B.3 — Inline compaction hook (optional)

If `compaction.inline` is true, call `compact_generation_chain` at the end
of `publish_generation`.  This keeps the chain bounded without cron, at
the cost of occasional latency spikes on publish.

### B.4 — CLI subcommand

Add `git-cache compact [--repo <repo>] [--all] [--dry-run]` to
`git-cache-cli` that invokes compaction for one or all repos.  This is
the primary intended trigger (via cron or manual).

### B.5 — Cleanup of orphaned generations

After compaction, the old generation manifests and bundles must be deleted.
This requires listing generations for the repo and removing any that are
not reachable from the new head.  Use `list_prefix` on
`repos/{repo}/generations/` to find candidates.

---

## Stream C — Git Wrapper (`git-cache-git`)

New methods on the `Git` struct.

### C.1 — `bundle_create_incremental`

```rust
/// Create a bundle containing all objects reachable from `--all`
/// but NOT reachable from any of the `exclude_tips`.
///
/// Equivalent to: `git bundle create <path> --all ^<tip1> ^<tip2> ...`
pub async fn bundle_create_incremental(
    &self,
    repo_dir: &Path,
    bundle_path: &Path,
    exclude_tips: &[CommitSha],
) -> Result<GitOutput> {
    // Validate each tip via reject_revision_arg.
    for tip in exclude_tips {
        reject_revision_arg(tip.as_str())?;
    }
    let mut args: Vec<String> = vec![
        "bundle".into(),
        "create".into(),
        path_to_str(bundle_path)?.into(),
        "--all".into(),
    ];
    for tip in exclude_tips {
        args.push(format!("^{}", tip.as_str()));
    }
    self.run_vec(Some(repo_dir), args).await
}
```

### C.2 — `Git::run_vec` (if not already present)

A variant of `run` that accepts `Vec<String>` instead of a fixed-size
array.  Needed because the arg count for incremental bundles is dynamic.

Check if `run` already accepts `IntoIterator<Item = impl AsRef<str>>` —
if so, `bundle_create_incremental` can call `run` directly.

### C.3 — Tests

- `bundle_create_incremental` with valid exclude tips produces a smaller bundle.
- `bundle_create_incremental` with dash-prefixed tip is rejected.
- `bundle_create_incremental` with empty exclude list behaves like `--all`.
- Round-trip: create full bundle → add commits → create incremental → fetch both
  → verify all objects present.

---

## Stream D — Integration Tests (`git-cache-domain` + `git-cache-api`)

After A, B, C are merged.

### D.1 — Unit tests for delta publish

- Publish generation 1 (full) → verify `parent_generation: None`.
- Publish generation 2 (delta) → verify `parent_generation: Some(gen1)`.
- Verify generation 2 bundle is smaller than generation 1 bundle.
- Hydrate from cold: delete local repo → hydrate gen2 → verify both
  gen1 and gen2 commits are present.

### D.2 — Unit tests for compaction

- Build a chain of N generations (N > threshold).
- Run `compact_generation_chain`.
- Verify: single root generation, all commits accessible, old bundles deleted.
- Verify: `RepoGenerationHead` updated, all `CommitManifest`s re-pointed.

### D.3 — End-to-end test

- Push commit A → materialize (full bundle, gen1).
- Push commit B → materialize (delta bundle, gen2, parent=gen1).
- Push commit C → materialize (delta bundle, gen3, parent=gen2).
- Evict hot cache.
- Materialize commit B → hydrates chain [gen1, gen2] → success.
- Run compaction → single gen4.
- Evict hot cache again.
- Materialize commit C → hydrates single gen4 → success.

### D.4 — Fallback test

- Publish gen1 with commit A.
- Evict hot cache (local repo deleted).
- Push commit B, attempt delta publish → delta fails (old tips not local).
- Verify fallback to full bundle, `parent_generation: None`.

---

## Dependency Graph

```
            ┌─────────────┐
            │  A-types     │   (merge first: types + helpers)
            │  ~0.5 day    │
            └──────┬───────┘
                   │
      ┌────────────┼────────────┐
      ▼            ▼            ▼
┌───────────┐ ┌──────────┐ ┌──────────┐
│ Stream A   │ │ Stream B │ │ Stream C │   (parallel)
│ delta pub  │ │ compactn │ │ git wrap │
│ ~1 day     │ │ ~1 day   │ │ ~0.5 day │
└─────┬──────┘ └────┬─────┘ └────┬─────┘
      │             │            │
      └─────────────┼────────────┘
                    ▼
            ┌─────────────┐
            │  Stream D    │   (integration tests)
            │  ~1 day      │
            └──────────────┘
```

Total estimate: ~2 days wall-clock (A-types + max(A,B,C) + D).

---

## Contracts Summary

| Contract | Crate | Signature |
|---|---|---|
| `RepoGenerationHead` | `git-cache-core` | Struct: `{ repo, generation, tip_commits, updated_at }` |
| `CompactionConfig` | `git-cache-core` | Struct: `{ chain_depth_threshold, inline }` |
| `read_repo_generation_head` | `git-cache-objectstore` | `async fn(store, repo) -> Result<Option<RepoGenerationHead>>` |
| `write_repo_generation_head` | `git-cache-objectstore` | `async fn(store, head) -> Result<()>` |
| `repo_generation_head_key` | `git-cache-objectstore` | `fn(repo) -> String` |
| `bundle_create_incremental` | `git-cache-git` | `async fn(repo_dir, bundle_path, exclude_tips) -> Result<GitOutput>` |
| `compact_generation_chain` | `git-cache-domain` | `async fn(repo) -> CoreResult<Option<GenerationId>>` |
| `CompactionReport` | `git-cache-domain` | Struct: `{ repo, old_chain_depth, old_generations, new_generation, bytes_reclaimed }` |

---

## Config Changes

```toml
# config/production.example.toml  (new section)
[compaction]
chain_depth_threshold = 10
inline = false
```

---

## Migration

No data migration needed.  Existing generations have `parent_generation: null`
and no `RepoGenerationHead`.  The delta-publish code treats a missing head
as "first generation" and produces a full bundle — identical to current behavior.
Compaction is purely additive.
