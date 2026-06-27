use super::*;
use git_cache_core::GIT_UPLOAD_PACK_SERVICE;

const SERVED_REPO_CONFIG_MARKER: &str = "git-cache-serving-config-v5";
/// Marker recording that the bare repo was hydrated with a filtered
/// (blobless) fetch and therefore cannot serve full-object clone shapes
/// until an unfiltered `--refetch` completes.
pub(super) const PARTIAL_HYDRATION_MARKER: &str = "git-cache-partial-hydration";
/// Marker recording that a previous full-history closure check failed after
/// the tip commit/tree were already present locally. Full-history hot-path
/// service must re-check and repair before exposing commits while this exists.
pub(super) const INCOMPLETE_CLOSURE_MARKER: &str = "git-cache-incomplete-full-closure";
const BLOBLESS_FETCH_FILTER: &str = "blob:none";

/// Local git config applied to every served bare repo. Marker-gated by
/// `SERVED_REPO_CONFIG_MARKER`; bump the marker version when changing this
/// set so existing repos pick up the new configuration.
pub(super) const SERVED_REPO_CONFIG: &[(&str, &str)] = &[
    ("uploadpack.allowAnySHA1InWant", "true"),
    ("uploadpack.allowFilter", "true"),
    ("uploadpack.allowReachableSHA1InWant", "true"),
    ("uploadpack.hideRefs", "refs/cache"),
    ("transfer.hideRefs", "refs/cache"),
    ("pack.useBitmaps", "false"),
    ("repack.writeBitmaps", "false"),
    ("core.multiPackIndex", "true"),
    ("pack.writeReverseIndex", "true"),
    ("pack.threads", "0"),
    ("pack.deltaCacheSize", "256m"),
    ("core.deltaBaseCacheLimit", "512m"),
    ("fetch.unpackLimit", "1"),
    ("pack.compression", "1"),
    ("core.compression", "1"),
];

#[cfg(test)]
const DIRECT_FSCK_DELAY: StdDuration = StdDuration::from_millis(20);
#[cfg(not(test))]
const DIRECT_FSCK_DELAY: StdDuration = StdDuration::from_secs(30);

#[cfg(test)]
const SERVING_MAINTENANCE_DELAY: StdDuration = StdDuration::from_millis(20);
#[cfg(not(test))]
const SERVING_MAINTENANCE_DELAY: StdDuration = StdDuration::from_secs(60);

/// Skip the serving repack when the repo is already tidy. A geometric repack
/// keeps the pack count small, so more packs than this means new data has
/// accumulated; many loose objects likewise warrant a roll-up. `count-objects`
/// is read-only, so this gate is evaluated before taking the mutation lock.
const SERVING_MAINTENANCE_PACK_THRESHOLD: u64 = 3;
const SERVING_MAINTENANCE_LOOSE_THRESHOLD: u64 = 1000;

pub(super) enum DirectFetchedWantKind {
    Commit,
    NonCommit,
}

/// How a shallow cache repo must be extended before it can serve a want.
#[derive(Debug, Clone, Copy)]
enum HistoryExtension {
    /// Remove the shallow boundary entirely (full-history serving).
    Unshallow,
    /// Extend the shallow boundary by N commits (bounded `--deepen=N`).
    Deepen(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UploadPackFilter {
    BlobNone,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct UploadPackIntent {
    pub(super) wants: Vec<CommitSha>,
    pub(super) filter: Option<UploadPackFilter>,
    pub(super) depth: Option<u32>,
    pub(super) deepen_since: Option<u64>,
    pub(super) deepen_not: Vec<String>,
    pub(super) shallow: Vec<CommitSha>,
}

impl UploadPackIntent {
    pub(super) fn deepens_existing_shallow_boundary(&self) -> bool {
        !self.shallow.is_empty()
            && (self.depth.is_some() || self.deepen_since.is_some() || !self.deepen_not.is_empty())
    }
}

#[derive(Debug, Clone, Copy)]
struct DirectFetchOptions {
    filter: Option<&'static str>,
    depth: Option<u32>,
    deepen_existing_shallow: bool,
    hydrate_manifests: bool,
    refetch: bool,
    unshallow: bool,
    /// Relative `--deepen=N`: extend an existing shallow boundary by `N`
    /// commits. Set only on the bounded-deepen serving path; mutually
    /// exclusive with `depth` and `unshallow`.
    deepen: Option<u32>,
}

impl Default for DirectFetchOptions {
    fn default() -> Self {
        Self {
            filter: None,
            depth: None,
            deepen_existing_shallow: false,
            hydrate_manifests: true,
            refetch: false,
            unshallow: false,
            deepen: None,
        }
    }
}

impl DirectFetchOptions {
    fn from_intent(intent: &UploadPackIntent) -> Self {
        Self {
            filter: match intent.filter {
                Some(UploadPackFilter::BlobNone) => Some(BLOBLESS_FETCH_FILTER),
                None => None,
            },
            depth: intent.depth,
            deepen_existing_shallow: intent.deepens_existing_shallow_boundary(),
            // Filtered (blobless) intents skip per-want manifest hydration:
            // their wants are commits the batched fetch hydrates directly, or
            // lazy-fetched blobs (tens of thousands per checkout) for which a
            // serial object-store lookup per want would stall the request.
            hydrate_manifests: intent.filter.is_none(),
            refetch: false,
            unshallow: false,
            deepen: None,
        }
    }

    #[cfg(test)]
    fn from_blobless(blobless_fetch: bool) -> Self {
        Self {
            filter: blobless_fetch.then_some(BLOBLESS_FETCH_FILTER),
            depth: None,
            deepen_existing_shallow: false,
            hydrate_manifests: !blobless_fetch,
            refetch: false,
            unshallow: false,
            deepen: None,
        }
    }

    fn blobless_fetch(self) -> bool {
        self.filter == Some(BLOBLESS_FETCH_FILTER)
    }

    fn needs_full_object_history(self) -> bool {
        self.filter.is_none() && self.depth.is_none() && self.deepen.is_none()
    }

    fn without_manifest_hydration(mut self) -> Self {
        self.hydrate_manifests = false;
        self
    }

    fn with_refetch(mut self) -> Self {
        self.refetch = true;
        self
    }

    fn with_unshallow(mut self) -> Self {
        self.depth = None;
        self.deepen = None;
        self.unshallow = true;
        self
    }

    /// Switch to a bounded `--deepen=N` fetch: extend the cache's existing
    /// shallow boundary by `depth` commits instead of unshallowing it.
    fn with_bounded_deepen(mut self, depth: u32) -> Self {
        self.depth = None;
        self.unshallow = false;
        self.deepen = Some(depth);
        self
    }

    fn git_options(self) -> git_cache_git::FetchOptions<'static> {
        git_cache_git::FetchOptions {
            filter: self.filter,
            depth: self.depth,
            refetch: self.refetch,
            unshallow: self.unshallow,
            deepen: self.deepen,
        }
    }
}

impl Materializer {
    /// Fetch the upstream ref advertisement for a repo without downloading
    /// any objects.  Returns the structured ref data so the API layer can
    /// synthesize the pkt-line response directly, avoiding the need to
    /// materialise objects just for ls-remote.
    pub async fn upstream_refs(&self, repo: &RepoKey) -> CoreResult<UpstreamRefComparison> {
        let started = Instant::now();
        self.validate_host(repo)?;
        let upstream_url = self.upstream_url(repo)?;
        let ls = self
            .upstream_git(&upstream_url)?
            .ls_remote_heads(&upstream_url)
            .await?;

        info!(
            %repo,
            refs_count = ls.refs.len(),
            default_branch = ls.default_branch.as_deref().unwrap_or("<none>"),
            elapsed_ms = elapsed_ms(started),
            "fetched upstream refs for direct git advertisement"
        );
        Ok(UpstreamRefComparison {
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Ensure all wanted OIDs are available locally after repo access is
    /// proven.
    ///
    /// Authorization is repo-scoped. Object presence is cache state, not a
    /// second permission check: when a client asks for the commit advertised by
    /// the preceding `info/refs`, direct Git should behave as a read-through
    /// cache and import/hydrate the missing commit using the same
    /// request-scoped upstream auth. The repo cache is scoped by `RepoKey`;
    /// deployments that need stricter history isolation should use separate
    /// upstream repositories for truly separate data.
    #[cfg(test)]
    pub(super) async fn ensure_wants_available(
        &self,
        repo: &RepoKey,
        wants: &[String],
    ) -> CoreResult<()> {
        let object_ids = parse_want_strings(wants)?;
        Box::pin(self.ensure_wants_read_through(
            repo,
            &object_ids,
            &[],
            None,
            DirectFetchOptions::default(),
        ))
        .await
    }

    #[cfg(test)]
    pub(super) async fn ensure_wants_available_from_comparison(
        &self,
        repo: &RepoKey,
        wants: &[String],
        comparison: &UpstreamRefComparison,
        blobless_fetch: bool,
    ) -> CoreResult<()> {
        let object_ids = parse_want_strings(wants)?;
        Box::pin(self.ensure_wants_read_through(
            repo,
            &object_ids,
            &[],
            Some(comparison),
            DirectFetchOptions::from_blobless(blobless_fetch),
        ))
        .await
    }

    pub(super) async fn ensure_upload_pack_intent_available_from_comparison(
        &self,
        repo: &RepoKey,
        intent: &UploadPackIntent,
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        Box::pin(self.ensure_wants_read_through(
            repo,
            &intent.wants,
            &intent.shallow,
            Some(comparison),
            DirectFetchOptions::from_intent(intent),
        ))
        .await
    }

    pub(super) async fn ensure_upload_pack_intent_available(
        &self,
        repo: &RepoKey,
        intent: &UploadPackIntent,
    ) -> CoreResult<()> {
        Box::pin(self.ensure_wants_read_through(
            repo,
            &intent.wants,
            &intent.shallow,
            None,
            DirectFetchOptions::from_intent(intent),
        ))
        .await
    }

    async fn ensure_wants_read_through(
        &self,
        repo: &RepoKey,
        object_ids: &[CommitSha],
        deepen_from: &[CommitSha],
        comparison: Option<&UpstreamRefComparison>,
        fetch_options: DirectFetchOptions,
    ) -> CoreResult<()> {
        let started = Instant::now();
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        // A repo hydrated only by filtered (blobless) fetches has commits and
        // trees but no blobs. Readiness checks cannot see blob completeness,
        // so full-object requests against such a repo would die in server
        // pack-objects mid-stream. Force a `--refetch` covering every want so
        // git re-downloads full objects despite the local (partial) haves.
        let partial_marker = repo_dir.join(PARTIAL_HYDRATION_MARKER);
        let closure_marker = repo_dir.join(INCOMPLETE_CLOSURE_MARKER);
        let mut force_refetch =
            !fetch_options.blobless_fetch() && fs::try_exists(&partial_marker).await?;
        let closure_marker_present =
            fetch_options.needs_full_object_history() && fs::try_exists(&closure_marker).await?;
        // A blobless hydration followed by client checkout blob storms often
        // leaves the depth-1 snapshot of a tip fully present locally even
        // though the repo as a whole is partial. For single-commit-deep
        // intents the served pack is exactly each want's own snapshot, so a
        // local completeness walk can prove the refetch unnecessary — at the
        // cost of one rev-list per want instead of re-downloading the pack
        // from upstream. Deeper or full-history intents keep the refetch:
        // their pack shape cannot be cheaply proven complete.
        if force_refetch
            && fetch_options.depth == Some(1)
            && self
                .depth1_snapshots_complete_no_lazy(&repo_dir, object_ids)
                .await?
        {
            force_refetch = false;
            info!(
                %repo,
                wants = object_ids.len(),
                "partially hydrated repo already holds complete depth-1 snapshots; skipping forced refetch"
            );
        }
        let fetch_options = if force_refetch {
            fetch_options.with_refetch()
        } else {
            fetch_options
        };
        if force_refetch {
            info!(
                %repo,
                depth = fetch_options.depth,
                "repo partially hydrated (blobless); forcing full refetch of wants"
            );
        }
        // A repo hydrated only by depth-limited fetches is shallow and cannot
        // serve more history than it holds. The batched fetch must extend the
        // cache before serving, in one of two shapes:
        //
        // - Full-history want (no client depth): the served pack would
        //   otherwise stop at the cache's shallow boundary while the client
        //   repo is not marked shallow — a silently corrupt clone. Unshallow.
        // - `fetch --deepen=N` over an existing client shallow boundary: the
        //   client stays shallow, so the cache only needs N more commits of
        //   ancestry, not the whole history. A bounded `--deepen=N` avoids
        //   streaming an entire LLVM/Linux-sized history for a small deepen.
        //   A shallow→shallow deepen is always self-consistent (upload-pack
        //   reports the served boundary back to the client), so the only cost
        //   of under-deepening is a client whose history is shorter than it
        //   asked for; in the production `proxy_on_miss` path the client is
        //   served by the upstream proxy and this fetch only warms the cache.
        //
        // The bounded path is reserved for clean shallow caches.
        // `force_refetch` (a full-object client hitting a partial/blobless
        // cache) keeps unshallowing, since its boundary and object-completeness
        // reasoning is murkier; note a *blobless* deepen intent leaves
        // `force_refetch` false and so still takes the bounded path, which is
        // fine (blobless→blobless serving is self-consistent). deepen-since/
        // deepen-not and `--unshallow`-equivalent (infinite depth) requests
        // have no bounded commit count to deepen by. Lazy exact-oid fetches are
        // unaffected (`fetch_objects` never moves the boundary), so blobless
        // checkout blob storms stay cheap.
        let cache_repo_is_shallow = fs::try_exists(repo_dir.join("shallow")).await?;
        let history_extension = if !cache_repo_is_shallow {
            None
        } else if fetch_options.depth.is_none() {
            Some(HistoryExtension::Unshallow)
        } else if fetch_options.deepen_existing_shallow {
            // `--depth=M` and `--deepen=M` both arrive as `deepen M` on the
            // wire (the parser ignores the `deepen-relative` capability), so an
            // absolute `--depth=M` re-request from an already-shallow client
            // also lands here and runs a *relative* `git fetch --deepen=M`. The
            // cache may over-fetch by up to M, but its own upload-pack still
            // serves the client's true (absolute or relative) depth semantics,
            // so the served result stays correct either way.
            match fetch_options.depth {
                Some(depth) if depth <= MAX_LOCAL_DEPTH_WINDOW_PROOF && !force_refetch => {
                    // A stateless deepen runs multiple upload-pack rounds; only
                    // deepen the cache on the round that actually needs more
                    // history, so the boundary advances by N once rather than
                    // compounding to 2N, 3N, ... across rounds. The check is
                    // all-or-nothing over `deepen_from`, while the deepen below
                    // applies to every fetched refspec, so a request mixing a
                    // covered branch with an under-covered one re-deepens the
                    // covered branch by an extra N — bounded and convergent, not
                    // corrupt.
                    if self
                        .deepen_boundary_satisfied(&repo_dir, deepen_from, depth)
                        .await?
                    {
                        None
                    } else {
                        Some(HistoryExtension::Deepen(depth))
                    }
                }
                _ => Some(HistoryExtension::Unshallow),
            }
        } else {
            None
        };
        let needs_history_extension = history_extension.is_some();
        let fetch_options = match history_extension {
            Some(HistoryExtension::Unshallow) => {
                info!(
                    %repo,
                    deepen_existing_shallow = fetch_options.deepen_existing_shallow,
                    "cache repo is shallow; forcing unshallow fetch before serving upload-pack wants"
                );
                fetch_options.with_unshallow()
            }
            Some(HistoryExtension::Deepen(depth)) => {
                info!(
                    %repo,
                    depth,
                    "cache repo is shallow; deepening cache by requested depth before serving deepen wants"
                );
                fetch_options.with_bounded_deepen(depth)
            }
            None => fetch_options,
        };
        let object_count = object_ids.len();
        // Classification must never trigger promisor lazy fetches: in a
        // partially hydrated repo each missing object would otherwise spawn
        // a serial upstream fetch.
        let object_types = self
            .state
            .git
            .cat_file_batch_types_no_lazy(&repo_dir, object_ids)
            .await?;
        let mut non_commit_wants = 0usize;
        let mut served_commits = 0usize;
        let mut hydrated_commits = 0usize;
        let mut fetched_commits = 0usize;
        let mut fetched_non_commit_wants = 0usize;
        let mut pending: Vec<CommitSha> = Vec::new();
        let mut pending_requires_refetch = force_refetch;
        let mut pending_needs_closure_verify = closure_marker_present;
        let mut checked_suspect_hot_closure = false;
        let mut suspect_hot_closure_complete = true;
        let mut verified_hydrated_generations: HashSet<GenerationId> = HashSet::new();

        for object_id in object_ids {
            if let Some(object_type) = object_types.get(object_id) {
                if object_type != "commit" {
                    non_commit_wants += 1;
                    continue;
                }

                if !force_refetch
                    && !needs_history_extension
                    && self.commit_tree_exists_no_lazy(&repo_dir, object_id).await
                {
                    if let Some(depth) = fetch_options.depth {
                        if depth > 1
                            && !self
                                .depth_window_ready_for_serving_no_lazy(&repo_dir, object_id, depth)
                                .await?
                        {
                            pending_requires_refetch = true;
                            pending.push(object_id.clone());
                            info!(
                                %repo,
                                commit = %object_id,
                                depth,
                                "direct git hot commit lacks requested depth window; falling back to read-through fetch"
                            );
                            continue;
                        }
                    }
                    if closure_marker_present && fetch_options.needs_full_object_history() {
                        checked_suspect_hot_closure = true;
                        if !self
                            .commit_history_complete_no_lazy(&repo_dir, object_id)
                            .await?
                        {
                            suspect_hot_closure_complete = false;
                            pending_requires_refetch = true;
                            pending_needs_closure_verify = true;
                            pending.push(object_id.clone());
                            warn!(
                                %repo,
                                commit = %object_id,
                                "marked suspect hot commit has incomplete full-history closure; falling back to read-through fetch"
                            );
                            continue;
                        }
                    }
                    self.expose_served_commit(&repo_dir, object_id).await?;
                    served_commits += 1;
                    continue;
                }
            }

            if !force_refetch && !needs_history_extension && fetch_options.hydrate_manifests {
                if let Some(manifest) = self.get_commit_manifest(repo, object_id).await? {
                    if manifest.complete {
                        match Box::pin(self.hydrate_commit_in_repo(&repo_dir, &manifest)).await {
                            Ok(()) => {
                                if !fetch_options.needs_full_object_history()
                                    || verified_hydrated_generations
                                        .contains(&manifest.generation)
                                    || self
                                        .commit_history_complete_no_lazy(&repo_dir, object_id)
                                        .await?
                                {
                                    verified_hydrated_generations.insert(manifest.generation);
                                    self.expose_served_commit(&repo_dir, object_id).await?;
                                    hydrated_commits += 1;
                                    continue;
                                }
                                fs::write(&closure_marker, b"incomplete\n").await?;
                                pending_needs_closure_verify = true;
                                pending_requires_refetch = true;
                                warn!(
                                    %repo,
                                    commit = %object_id,
                                    generation = %manifest.generation,
                                    "commit manifest hydrated incomplete full-history closure; falling back to read-through fetch"
                                );
                            }
                            // A commit manifest can point at a generation that
                            // was swept or repointed by another node; fall
                            // through to the read-through fetch instead of
                            // failing the whole request.
                            Err(err)
                                if super::planning::exact_hydrate_error_allows_upstream_fallback(
                                    &err,
                                ) =>
                            {
                                warn!(
                                    %repo,
                                    commit = %object_id,
                                    generation = %manifest.generation,
                                    %err,
                                    "commit manifest hydrate missed; falling back to read-through fetch"
                                );
                            }
                            Err(err) => return Err(err),
                        }
                    }
                }
            }

            if !self.state.config.git_remote.commit_read_through {
                return Err(GitCacheError::NotFound(format!(
                    "commit `{object_id}` is not available and read-through is disabled"
                )));
            }

            pending.push(object_id.clone());
        }

        if !pending.is_empty() {
            let _mutation_lock = self.lock_repo_mutation(repo).await?;
            let fetch_options = if pending_requires_refetch && !needs_history_extension {
                fetch_options.with_refetch()
            } else {
                fetch_options
            };
            // A fresh clone wants the tip of every advertised ref, so missing
            // wants must be fetched in batched upstream invocations: one fetch
            // for all advertised refspecs plus one for raw SHAs, instead of one
            // subprocess per want.
            let upstream_url = self.upstream_url(repo)?;
            let upstream_git = self.upstream_git(&upstream_url)?;
            let mut fetched_commit_wants: Vec<CommitSha> = Vec::new();

            let fetched_all_heads = self
                .batched_read_through_fetch(
                    repo,
                    &repo_dir,
                    &upstream_url,
                    &upstream_git,
                    &pending,
                    comparison,
                    fetch_options,
                )
                .await?;

            let mut missing: Vec<CommitSha> = Vec::new();
            for object_id in &pending {
                match self.prepare_fetched_direct_want(&repo_dir, object_id).await {
                    Ok(DirectFetchedWantKind::Commit) => {
                        fetched_commits += 1;
                        fetched_commit_wants.push(object_id.clone());
                    }
                    Ok(DirectFetchedWantKind::NonCommit) => fetched_non_commit_wants += 1,
                    Err(_) => missing.push(object_id.clone()),
                }
            }

            if !missing.is_empty() {
                // Wants the batched fetch did not cover (e.g. an advertised
                // branch moved between the GET advertisement and this POST)
                // are retried as exact-SHA fetches, then as an all-heads fetch.
                warn!(
                    %repo,
                    missing_count = missing.len(),
                    "direct git batched fetch did not cover all wants; retrying as exact-SHA fetch"
                );
                if let Err(err) = upstream_git
                    .fetch_objects(
                        &repo_dir,
                        &upstream_url,
                        &missing,
                        fetch_options.git_options(),
                    )
                    .await
                {
                    if fetched_all_heads {
                        return Err(GitCacheError::NotFound(format!(
                            "objects could not be fetched from upstream: {err}"
                        )));
                    }
                    warn!(
                        %repo,
                        missing_count = missing.len(),
                        %err,
                        "direct git exact-SHA retry fetch failed; falling back to all-heads fetch"
                    );
                    upstream_git
                        .fetch_all_heads(&repo_dir, &upstream_url, fetch_options.git_options())
                        .await?;
                }
                for object_id in &missing {
                    match self.prepare_fetched_direct_want(&repo_dir, object_id).await {
                        Ok(DirectFetchedWantKind::Commit) => {
                            fetched_commits += 1;
                            fetched_commit_wants.push(object_id.clone());
                        }
                        Ok(DirectFetchedWantKind::NonCommit) => fetched_non_commit_wants += 1,
                        Err(err) => {
                            return Err(GitCacheError::NotFound(format!(
                                "object `{object_id}` could not be fetched from upstream: {err}"
                            )));
                        }
                    }
                }
            }

            let mut verified_full_closure = true;
            if pending_needs_closure_verify && fetch_options.needs_full_object_history() {
                for object_id in &fetched_commit_wants {
                    if !self
                        .commit_history_complete_no_lazy(&repo_dir, object_id)
                        .await?
                    {
                        verified_full_closure = false;
                        break;
                    }
                }
                if verified_full_closure {
                    fs::remove_file(&closure_marker).await.ok();
                } else {
                    fs::write(&closure_marker, b"incomplete\n").await?;
                    return Err(GitCacheError::Internal(
                        "fetched full-history wants without complete object closure".into(),
                    ));
                }
            }

            if fetch_options.blobless_fetch() {
                fs::write(&partial_marker, b"blobless\n").await?;
            } else if force_refetch && fetch_options.depth.is_none() {
                // An unfiltered, undepthed refetch re-downloads full objects
                // for every requested want; the repo can serve full-object
                // shapes again.
                fs::remove_file(&partial_marker).await.ok();
                info!(%repo, "cleared partial hydration marker after full refetch");
            }

            // One repo-wide fsck covers every commit fetched by this request.
            if fetched_commits > 0 || fetched_non_commit_wants > 0 {
                if let Some(first) = pending.first() {
                    self.enqueue_direct_fsck(repo.clone(), repo_dir.to_path_buf(), first.clone());
                }
                self.enqueue_serving_maintenance(repo.clone(), repo_dir.to_path_buf());
            }
        }

        if checked_suspect_hot_closure && suspect_hot_closure_complete {
            fs::remove_file(&closure_marker).await.ok();
        }

        info!(
            %repo,
            wants_count = object_count,
            non_commit_wants,
            served_commits,
            hydrated_commits,
            fetched_commits,
            fetched_non_commit_wants,
            blobless_fetch = fetch_options.blobless_fetch(),
            depth = fetch_options.depth,
            hydrate_manifests = fetch_options.hydrate_manifests,
            elapsed_ms = elapsed_ms(started),
            "ensured direct git wants via read-through"
        );
        Ok(())
    }

    /// Shared batched read-through fetch core: classify pending wants into
    /// advertised-branch refspecs (when an upstream ref comparison is
    /// available) and raw exact-SHA objects, then hydrate them with at most
    /// two upstream fetches, falling back to a single all-heads fetch on
    /// failure. Returns whether the all-heads fallback ran.
    ///
    /// Direct Git read-through and the proxy-on-miss background warm both
    /// flow through this core; `/materialize` branch hydration shares the
    /// same `branch_cache_refspec` construction so upstream fetch behavior
    /// stays aligned across paths.
    #[allow(clippy::too_many_arguments)]
    async fn batched_read_through_fetch(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        upstream_url: &str,
        upstream_git: &git_cache_git::Git,
        pending: &[CommitSha],
        comparison: Option<&UpstreamRefComparison>,
        fetch_options: DirectFetchOptions,
    ) -> CoreResult<bool> {
        let mut refspecs: Vec<String> = Vec::new();
        let mut raw_objects: Vec<CommitSha> = Vec::new();
        for object_id in pending {
            match comparison
                .and_then(|comparison| comparison.branch_for_commit(object_id))
                .map(git_cache_git::branch_cache_refspec)
            {
                Some(Ok(refspec)) => refspecs.push(refspec),
                Some(Err(err)) => {
                    warn!(
                        %repo,
                        %object_id,
                        %err,
                        "advertised branch name failed refspec validation; fetching as raw object"
                    );
                    raw_objects.push(object_id.clone());
                }
                None => raw_objects.push(object_id.clone()),
            }
        }
        refspecs.sort();
        refspecs.dedup();

        info!(
            %repo,
            pending_wants = pending.len(),
            refspec_count = refspecs.len(),
            raw_object_count = raw_objects.len(),
            depth = fetch_options.depth,
            blobless_fetch = fetch_options.blobless_fetch(),
            "direct git batched read-through fetch for wanted commits"
        );

        let mut fetched_all_heads = false;
        if !refspecs.is_empty() {
            if let Err(err) = upstream_git
                .fetch_refspecs(
                    repo_dir,
                    upstream_url,
                    &refspecs,
                    fetch_options.git_options(),
                )
                .await
            {
                warn!(
                    %repo,
                    refspec_count = refspecs.len(),
                    %err,
                    "direct git batched advertised-ref fetch failed; falling back to all-heads fetch"
                );
                upstream_git
                    .fetch_all_heads(repo_dir, upstream_url, fetch_options.git_options())
                    .await?;
                fetched_all_heads = true;
            }
        }
        if !raw_objects.is_empty() {
            if let Err(err) = upstream_git
                .fetch_objects(
                    repo_dir,
                    upstream_url,
                    &raw_objects,
                    fetch_options.git_options(),
                )
                .await
            {
                warn!(
                    %repo,
                    raw_object_count = raw_objects.len(),
                    %err,
                    "direct git batched raw-object fetch failed; falling back to all-heads fetch"
                );
                if !fetched_all_heads {
                    upstream_git
                        .fetch_all_heads(repo_dir, upstream_url, fetch_options.git_options())
                        .await?;
                    fetched_all_heads = true;
                }
            }
        }
        Ok(fetched_all_heads)
    }

    pub(super) async fn prepare_fetched_direct_want(
        &self,
        repo_dir: &FsPath,
        object_id: &CommitSha,
    ) -> CoreResult<DirectFetchedWantKind> {
        let object_types = self
            .state
            .git
            .cat_file_batch_types_no_lazy(repo_dir, std::slice::from_ref(object_id))
            .await?;
        let Some(object_type) = object_types.get(object_id).map(String::as_str) else {
            return Err(GitCacheError::NotFound(format!(
                "object `{object_id}` not found after upstream fetch"
            )));
        };

        if object_type != "commit" {
            return Ok(DirectFetchedWantKind::NonCommit);
        }

        if !self
            .commit_ready_for_serving_no_lazy(repo_dir, object_id)
            .await
        {
            return Err(GitCacheError::NotFound(format!(
                "commit `{object_id}` not found or incomplete after upstream fetch"
            )));
        }
        self.expose_served_commit(repo_dir, object_id).await?;

        // Keep a hidden ref so the fetched commit remains reachable in the
        // shared bare repo. The ref is hidden from clients by
        // configure_served_repo.
        let cache_ref = format!("refs/cache/commits/{object_id}");
        self.state
            .git
            .update_ref(repo_dir, &cache_ref, object_id.as_str())
            .await?;

        Ok(DirectFetchedWantKind::Commit)
    }

    pub(super) fn enqueue_direct_fsck(&self, repo: RepoKey, repo_dir: PathBuf, commit: CommitSha) {
        {
            let Ok(mut inflight) = self.state.direct_fsck_inflight.lock() else {
                warn!(%repo, "direct fsck in-flight lock poisoned; skipping");
                return;
            };
            if !inflight.insert(repo_dir.clone()) {
                // A connectivity fsck for this repo is already queued or
                // running; enqueuing another would only stampede duplicate
                // IO/CPU over the same objects on a large repo.
                return;
            }
        }
        let materializer = self.clone();
        info!(
            %repo,
            %commit,
            delay_ms = DIRECT_FSCK_DELAY.as_millis(),
            "queued direct git background fsck"
        );
        tokio::spawn(async move {
            tokio::time::sleep(DIRECT_FSCK_DELAY).await;
            let started = Instant::now();
            // Serialize the connectivity check with fetch/repack/commit-graph
            // through the per-repo mutation lock: an fsck racing a shallow
            // boundary rewrite wastes work and competes for IO, and acquiring
            // the guard also clears any stale lock left by a prior crash before
            // fsck walks the object graph.
            let result = async {
                let _mutation_lock = materializer.lock_repo_mutation(&repo).await?;
                materializer.state.git.fsck(&repo_dir).await?;
                CoreResult::Ok(())
            }
            .await;
            match result {
                Ok(()) => info!(
                    %repo,
                    %commit,
                    elapsed_ms = elapsed_ms(started),
                    "direct git background fsck finished"
                ),
                Err(err) => warn!(
                    %repo,
                    %commit,
                    %err,
                    elapsed_ms = elapsed_ms(started),
                    "direct git background fsck failed"
                ),
            }
            if let Ok(mut inflight) = materializer.state.direct_fsck_inflight.lock() {
                inflight.remove(&repo_dir);
            }
        });
    }

    /// Debounced background maintenance that keeps served repos compact. On
    /// git >= 2.32 it runs an incremental geometric `repack` (writes a
    /// multi-pack-index; cost proportional to newly fetched data) plus a
    /// `--split` commit-graph update, both at low CPU/IO priority so concurrent
    /// serves and read-through fetches are unaffected. The repack is skipped
    /// entirely when `count-objects` shows the repo is already tidy (checked
    /// before taking the mutation lock). Direct upload-pack disables bitmap
    /// traversal for correctness, so maintenance never writes bitmap indexes.
    /// At most one maintenance run per repo is queued or running at a time.
    pub(super) fn enqueue_serving_maintenance(&self, repo: RepoKey, repo_dir: PathBuf) {
        {
            let Ok(mut inflight) = self.state.serving_maintenance_inflight.lock() else {
                warn!(%repo, "serving maintenance in-flight lock poisoned; skipping");
                return;
            };
            if !inflight.insert(repo_dir.clone()) {
                return;
            }
        }
        info!(
            %repo,
            delay_ms = SERVING_MAINTENANCE_DELAY.as_millis(),
            "queued direct git serving maintenance (repack + commit-graph)"
        );
        let materializer = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SERVING_MAINTENANCE_DELAY).await;
            let started = Instant::now();
            // Background maintenance yields CPU/IO to concurrent serves.
            let maintenance_git = materializer.state.git.clone().with_low_priority();
            let result = async {
                // Decide whether a repack is worthwhile before taking the
                // mutation lock: `count-objects` is read-only, so this never
                // blocks concurrent read-through fetches. Default to repacking
                // when the count cannot be read.
                let needs_repack = match maintenance_git.count_objects(&repo_dir).await {
                    Ok(counts) => {
                        counts.packs > SERVING_MAINTENANCE_PACK_THRESHOLD
                            || counts.loose_objects > SERVING_MAINTENANCE_LOOSE_THRESHOLD
                    }
                    Err(_) => true,
                };
                let _mutation_lock = materializer.lock_repo_mutation(&repo).await?;
                if needs_repack {
                    maintenance_git.repack_for_serving(&repo_dir).await?;
                }
                maintenance_git.commit_graph_write(&repo_dir).await?;
                CoreResult::Ok(needs_repack)
            }
            .await;
            match result {
                Ok(repacked) => info!(
                    %repo,
                    repacked,
                    elapsed_ms = elapsed_ms(started),
                    "direct git serving maintenance finished"
                ),
                Err(err) => warn!(
                    %repo,
                    %err,
                    elapsed_ms = elapsed_ms(started),
                    "direct git serving maintenance failed"
                ),
            }
            if let Ok(mut inflight) = materializer.state.serving_maintenance_inflight.lock() {
                inflight.remove(&repo_dir);
            }
        });
    }

    fn direct_git_repo_cache_miss(repo: &RepoKey) -> GitCacheError {
        GitCacheError::UpstreamUnavailable(format!(
            "repo `{repo}` is not available in the local cache"
        ))
    }

    /// Configure a bare repo for serving via the direct Git remote:
    /// - `uploadpack.allowAnySHA1InWant=true`
    /// - `uploadpack.allowReachableSHA1InWant=true`
    /// - `uploadpack.allowFilter=true`
    /// - `uploadpack.hideRefs=refs/cache`
    /// - `transfer.hideRefs=refs/cache`
    pub(super) async fn configure_served_repo(&self, repo_dir: &FsPath) -> CoreResult<()> {
        let marker = repo_dir.join(SERVED_REPO_CONFIG_MARKER);
        if fs::try_exists(&marker).await? {
            return Ok(());
        }

        for (key, value) in SERVED_REPO_CONFIG {
            self.state.git.set_config(repo_dir, key, value).await?;
        }
        fs::write(marker, b"configured\n").await?;
        Ok(())
    }

    pub async fn optimize_repo_for_serving(&self, repo: &RepoKey) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _locks = self.lock_repo_for_mutation(repo).await?;
        self.configure_served_repo(&repo_dir).await?;
        self.state.git.repack_for_serving(&repo_dir).await?;
        Ok(())
    }

    /// Prepare direct Git wants from the cache without contacting upstream.
    ///
    /// Proxy-on-miss uses this before falling through to the upstream proxy:
    /// EBS-local objects and complete object-store commit manifests both count
    /// as warm. If this returns `false`, the existing read-through path still
    /// remains the authoritative warmer/import path.
    pub async fn prepare_upload_pack_from_cache(
        &self,
        repo: &RepoKey,
        body: &Bytes,
    ) -> CoreResult<bool> {
        let started = Instant::now();
        let intent = parse_upload_pack_intent(body)?;
        let repo_dir = self.ensure_repo_dir(repo).await?;

        if intent.wants.is_empty() {
            return Ok(true);
        }

        // A blobless-hydrated repo cannot serve full-object shapes; decline
        // so proxy-on-miss streams from upstream while the background warm
        // refetches full objects into the cache.
        if intent.filter.is_none()
            && fs::try_exists(repo_dir.join(PARTIAL_HYDRATION_MARKER)).await?
        {
            // Exception: a depth-1 intent whose tips' snapshots are fully
            // present locally (e.g. filled by checkout blob storms after the
            // blobless hydration) can be served from cache.
            if intent.depth == Some(1)
                && self
                    .depth1_snapshots_complete_no_lazy(&repo_dir, &intent.wants)
                    .await?
            {
                info!(
                    %repo,
                    wants_count = intent.wants.len(),
                    "partially hydrated repo holds complete depth-1 snapshots; serving from cache"
                );
            } else {
                info!(
                    %repo,
                    wants_count = intent.wants.len(),
                    "direct git cache prepare declined: repo partially hydrated (blobless), request needs full objects"
                );
                return Ok(false);
            }
        }

        // A shallow cache repo (depth-limited hydration) cannot serve
        // full-history commit wants: the pack would stop at the shallow
        // boundary while the client repo is not marked shallow — a silently
        // corrupt clone. Lazy exact-oid (blob/tree) fetches are unaffected.
        let shallow_repo_cannot_serve_locally = fs::try_exists(repo_dir.join("shallow")).await?
            && (intent.depth.is_none() || intent.deepens_existing_shallow_boundary());

        let _repo_lock = self.lock_repo(repo).await?;
        let object_types = self
            .state
            .git
            .cat_file_batch_types_no_lazy(&repo_dir, &intent.wants)
            .await?;
        let wants_count = intent.wants.len();
        let mut served_commits = 0usize;
        let mut served_non_commit_wants = 0usize;
        for object_id in &intent.wants {
            if let Some(object_type) = object_types.get(object_id).map(String::as_str) {
                if object_type != "commit" {
                    served_non_commit_wants += 1;
                    continue;
                }
                if shallow_repo_cannot_serve_locally {
                    info!(
                        %repo,
                        commit = %object_id,
                        wants_count,
                        deepen_existing_shallow = intent.deepens_existing_shallow_boundary(),
                        "direct git cache prepare declined: repo is shallow, commit want needs more history"
                    );
                    return Ok(false);
                }
                if let Some(depth) = intent.depth {
                    if depth > 1
                        && !self
                            .depth_window_ready_for_serving_no_lazy(&repo_dir, object_id, depth)
                            .await?
                    {
                        info!(
                            %repo,
                            commit = %object_id,
                            wants_count,
                            depth,
                            "direct git cache prepare declined: commit want lacks requested depth window"
                        );
                        return Ok(false);
                    }
                }
                if self.commit_tree_exists_no_lazy(&repo_dir, object_id).await {
                    self.expose_served_commit(&repo_dir, object_id).await?;
                    served_commits += 1;
                    continue;
                }
            }

            // This readiness check is intentionally EBS-local. Pulling a
            // generation bundle from object storage can require importing a
            // multi-GB pack before serving a tiny shallow/blobless clone; with
            // proxy-on-miss requested, an EBS miss should proxy immediately and
            // let the background warm path fill the local repo.
            info!(
                %repo,
                commit = %object_id,
                wants_count,
                served_commits,
                served_non_commit_wants,
                elapsed_ms = elapsed_ms(started),
                "direct git cache prepare missed local object"
            );
            return Ok(false);
        }

        info!(
            %repo,
            wants_count,
            served_commits,
            served_non_commit_wants,
            elapsed_ms = elapsed_ms(started),
            "direct git cache prepare satisfied upload-pack wants"
        );
        Ok(true)
    }

    pub async fn warm_upload_pack(
        &self,
        repo: &RepoKey,
        body: &Bytes,
        comparison: Option<&UpstreamRefComparison>,
    ) -> CoreResult<()> {
        let intent = parse_upload_pack_intent(body)?;
        if intent.wants.is_empty() {
            return Ok(());
        }

        // Proxy background warming is best-effort and should mirror the
        // client's shallow/filter request. Avoid object-store generation
        // hydration here: a manifest hit can point at a multi-GB bundle, while
        // the upstream upload-pack request that just succeeded can usually
        // fetch the wanted commit as a tiny filtered pack.
        let fetch_options = DirectFetchOptions::from_intent(&intent).without_manifest_hydration();
        match comparison {
            Some(comparison) => {
                Box::pin(self.ensure_wants_read_through(
                    repo,
                    &intent.wants,
                    &intent.shallow,
                    Some(comparison),
                    fetch_options,
                ))
                .await
            }
            None => {
                Box::pin(self.ensure_wants_read_through(
                    repo,
                    &intent.wants,
                    &intent.shallow,
                    None,
                    fetch_options,
                ))
                .await
            }
        }
    }

    /// Handle a direct Git remote upload-pack request end-to-end:
    /// parse want lines, ensure objects are available, configure the repo,
    /// and spawn the upload-pack process for streaming.
    pub async fn handle_upload_pack(
        &self,
        repo: &RepoKey,
        body: &Bytes,
        comparison: Option<&UpstreamRefComparison>,
    ) -> CoreResult<UploadPackProcess> {
        let started = Instant::now();
        let intent = parse_upload_pack_intent(body)?;
        info!(
            %repo,
            wants_count = intent.wants.len(),
            cached_ref_proof = comparison.is_some(),
            blobless_fetch = intent.filter == Some(UploadPackFilter::BlobNone),
            depth = intent.depth,
            "direct git upload-pack preparation started"
        );
        if !intent.wants.is_empty() {
            let ensure_started = Instant::now();
            match comparison {
                Some(comparison) => {
                    Box::pin(self.ensure_upload_pack_intent_available_from_comparison(
                        repo, &intent, comparison,
                    ))
                    .await?;
                }
                None => Box::pin(self.ensure_upload_pack_intent_available(repo, &intent)).await?,
            }
            info!(
                %repo,
                wants_count = intent.wants.len(),
                elapsed_ms = elapsed_ms(ensure_started),
                "direct git upload-pack wants prepared"
            );
        }
        let repo_started = Instant::now();
        let repo_dir = self.repo_dir(repo);
        if !repo_dir.join("config").exists() {
            info!(%repo, "direct git upload-pack repo missing from local cache");
            return Err(Self::direct_git_repo_cache_miss(repo));
        }
        info!(
            %repo,
            elapsed_ms = elapsed_ms(repo_started),
            "direct git upload-pack repo directory ready"
        );
        let configure_started = Instant::now();
        self.configure_served_repo(&repo_dir).await?;
        info!(
            %repo,
            elapsed_ms = elapsed_ms(configure_started),
            "direct git upload-pack repo configured"
        );
        let spawn_started = Instant::now();
        let process = self
            .state
            .git
            .upload_pack_spawn(&repo_dir, body.clone())
            .await?;
        info!(
            %repo,
            spawn_elapsed_ms = elapsed_ms(spawn_started),
            elapsed_ms = elapsed_ms(started),
            "direct git upload-pack process ready"
        );
        Ok(process)
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamRefComparison {
    pub default_branch: Option<String>,
    pub all_upstream: HashMap<String, String>,
}

impl UpstreamRefComparison {
    pub fn branch_for_commit(&self, commit: &CommitSha) -> Option<&str> {
        if let Some(default_branch) = self.default_branch.as_deref() {
            if self
                .all_upstream
                .get(default_branch)
                .is_some_and(|sha| sha == commit.as_str())
            {
                return Some(default_branch);
            }
        }

        self.all_upstream
            .iter()
            .filter_map(|(branch, sha)| (sha == commit.as_str()).then_some(branch.as_str()))
            .min()
    }
}

/// Build a pkt-line formatted ref advertisement from upstream ref data.
///
/// This produces the same output as `git upload-pack --advertise-refs` but
/// without requiring the objects to exist locally.  The capability set
/// matches what a standard git 2.x upload-pack would emit.
pub fn synthesize_ref_advertisement(comparison: &UpstreamRefComparison) -> Vec<u8> {
    synthesize_ref_advertisement_inner(comparison)
}

fn synthesize_ref_advertisement_inner(comparison: &UpstreamRefComparison) -> Vec<u8> {
    let mut out = Vec::new();

    // Sort refs for deterministic output.
    let mut refs: Vec<(&String, &String)> = comparison.all_upstream.iter().collect();
    refs.sort_by_key(|(name, _)| name.as_str());

    // Only advertise symref when the default branch is actually present in
    // the upstream refs. A symref pointing at a non-existent ref confuses
    // git clients during clone.
    let resolved_default = comparison.default_branch.as_ref().and_then(|b| {
        comparison
            .all_upstream
            .get(b)
            .map(|sha| (b.as_str(), sha.as_str()))
    });

    let symref = resolved_default
        .map(|(b, _)| format!(" symref=HEAD:refs/heads/{b}"))
        .unwrap_or_default();

    let caps = format!(
        "multi_ack thin-pack side-band side-band-64k ofs-delta \
         shallow deepen-since deepen-not deepen-relative no-progress \
         include-tag multi_ack_detailed no-done \
         filter allow-tip-sha1-in-want allow-reachable-sha1-in-want{symref} \
         object-format=sha1 agent=git-cache/1.0"
    );

    // HEAD line (first ref includes capabilities).
    let mut first_ref_used_for_caps = false;
    if let Some((_, sha)) = resolved_default {
        let line = format!("{sha} HEAD\0{caps}\n");
        pkt_line(&mut out, &line);
    } else if let Some((name, sha)) = refs.first() {
        // No usable default branch: first sorted ref carries capabilities.
        let line = format!("{sha} refs/heads/{name}\0{caps}\n");
        pkt_line(&mut out, &line);
        first_ref_used_for_caps = true;
    }

    // Ref lines (skip the first if it was already emitted as the capability carrier).
    let skip_first = first_ref_used_for_caps;
    for (i, (name, sha)) in refs.iter().enumerate() {
        if skip_first && i == 0 {
            continue;
        }
        let line = format!("{sha} refs/heads/{name}\n");
        pkt_line(&mut out, &line);
    }

    // HEAD as a separate non-capability line (if default branch set).
    // Already emitted as the first capability line above, so only emit
    // ref lines here.

    out.extend_from_slice(b"0000");
    out
}

fn pkt_line(out: &mut Vec<u8>, data: &str) {
    let len = 4 + data.len();
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(data.as_bytes());
}

/// Frame a ref advertisement with the Git smart-HTTP service header.
pub fn frame_ref_advertisement(refs_output: &[u8]) -> Vec<u8> {
    let service_line = format!("# service={GIT_UPLOAD_PACK_SERVICE}\n");
    let mut framed = Vec::with_capacity(refs_output.len() + 4 + service_line.len() + 4);
    pkt_line(&mut framed, &service_line);
    framed.extend_from_slice(b"0000");
    framed.extend_from_slice(refs_output);
    framed
}

#[cfg(test)]
pub(super) fn upload_pack_requests_blobless_filter(body: &[u8]) -> bool {
    parse_upload_pack_intent(body)
        .map(|intent| intent.filter == Some(UploadPackFilter::BlobNone))
        .unwrap_or(false)
}

pub(super) fn parse_upload_pack_intent(body: &[u8]) -> CoreResult<UploadPackIntent> {
    let mut intent = UploadPackIntent::default();
    let mut error = None;
    visit_upload_pack_lines(body, |line| {
        if error.is_some() {
            return;
        }

        let line = line.trim();
        if let Some(rest) = line.strip_prefix("want ") {
            let oid = rest.split_whitespace().next().unwrap_or("");
            match CommitSha::parse(oid) {
                Ok(commit) => intent.wants.push(commit),
                Err(err) => error = Some(err),
            }
        } else if let Some(rest) = line.strip_prefix("filter ") {
            if rest.trim() == BLOBLESS_FETCH_FILTER {
                intent.filter = Some(UploadPackFilter::BlobNone);
            }
        } else if let Some(rest) = line.strip_prefix("deepen ") {
            match rest.trim().parse::<u32>() {
                Ok(depth) if depth > 0 => intent.depth = Some(depth),
                _ => {
                    error = Some(GitCacheError::Validation(format!(
                        "invalid upload-pack depth: {rest:?}"
                    )));
                }
            }
        } else if let Some(rest) = line.strip_prefix("deepen-since ") {
            match rest.trim().parse::<u64>() {
                Ok(value) => intent.deepen_since = Some(value),
                Err(_) => {
                    error = Some(GitCacheError::Validation(format!(
                        "invalid upload-pack deepen-since: {rest:?}"
                    )));
                }
            }
        } else if let Some(rest) = line.strip_prefix("deepen-not ") {
            let value = rest.trim();
            if !value.is_empty() {
                intent.deepen_not.push(value.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("shallow ") {
            match CommitSha::parse(rest.split_whitespace().next().unwrap_or("")) {
                Ok(commit) => intent.shallow.push(commit),
                Err(err) => error = Some(err),
            }
        }
    });

    if let Some(error) = error {
        return Err(error);
    }
    Ok(intent)
}

pub fn upload_pack_wants(body: &[u8]) -> CoreResult<Vec<CommitSha>> {
    Ok(parse_upload_pack_intent(body)?.wants)
}

pub fn upload_pack_requests_shallow_history(body: &[u8]) -> bool {
    parse_upload_pack_intent(body).is_ok_and(|intent| {
        intent.depth.is_some()
            || intent.deepen_since.is_some()
            || !intent.deepen_not.is_empty()
            || !intent.shallow.is_empty()
    })
}

#[cfg(test)]
fn parse_want_strings(wants: &[String]) -> CoreResult<Vec<CommitSha>> {
    wants
        .iter()
        .map(|want_sha| CommitSha::parse(want_sha.as_str()))
        .collect()
}

pub(super) fn visit_upload_pack_lines(body: &[u8], mut visit: impl FnMut(&str)) {
    let mut offset = 0;
    while offset + 4 <= body.len() {
        let hex = match std::str::from_utf8(&body[offset..offset + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let pkt_len = match usize::from_str_radix(hex, 16) {
            Ok(l) => l,
            Err(_) => break,
        };

        if pkt_len == 0 {
            offset += 4;
            continue;
        }
        if pkt_len < 4 || offset + pkt_len > body.len() {
            break;
        }

        let line = &body[offset + 4..offset + pkt_len];
        if let Ok(line_str) = std::str::from_utf8(line) {
            visit(line_str);
        }

        offset += pkt_len;
    }
}
