use super::*;

const SERVED_REPO_CONFIG_MARKER: &str = "git-cache-serving-config-v2";
/// Marker recording that the bare repo was hydrated with a filtered
/// (blobless) fetch and therefore cannot serve full-object clone shapes
/// until an unfiltered `--refetch` completes.
pub(super) const PARTIAL_HYDRATION_MARKER: &str = "git-cache-partial-hydration";
const BLOBLESS_FETCH_FILTER: &str = "blob:none";

/// Local git config applied to every served bare repo. Marker-gated by
/// `SERVED_REPO_CONFIG_MARKER`; bump the marker version when changing this
/// set so existing repos pick up the new configuration.
const SERVED_REPO_CONFIG: &[(&str, &str)] = &[
    ("uploadpack.allowAnySHA1InWant", "true"),
    ("uploadpack.allowFilter", "true"),
    ("uploadpack.allowReachableSHA1InWant", "true"),
    ("uploadpack.hideRefs", "refs/cache"),
    ("transfer.hideRefs", "refs/cache"),
    ("pack.useBitmaps", "true"),
    ("repack.writeBitmaps", "true"),
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

pub(super) enum DirectFetchedWantKind {
    Commit,
    NonCommit,
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

#[derive(Debug, Clone, Copy)]
struct DirectFetchOptions {
    filter: Option<&'static str>,
    depth: Option<u32>,
    hydrate_manifests: bool,
    refetch: bool,
    unshallow: bool,
}

impl Default for DirectFetchOptions {
    fn default() -> Self {
        Self {
            filter: None,
            depth: None,
            hydrate_manifests: true,
            refetch: false,
            unshallow: false,
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
            // Filtered (blobless) intents skip per-want manifest hydration:
            // their wants are commits the batched fetch hydrates directly, or
            // lazy-fetched blobs (tens of thousands per checkout) for which a
            // serial object-store lookup per want would stall the request.
            hydrate_manifests: intent.filter.is_none(),
            refetch: false,
            unshallow: false,
        }
    }

    #[cfg(test)]
    fn from_blobless(blobless_fetch: bool) -> Self {
        Self {
            filter: blobless_fetch.then_some(BLOBLESS_FETCH_FILTER),
            depth: None,
            hydrate_manifests: !blobless_fetch,
            refetch: false,
            unshallow: false,
        }
    }

    fn blobless_fetch(self) -> bool {
        self.filter == Some(BLOBLESS_FETCH_FILTER)
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
        self.unshallow = true;
        self
    }

    fn git_options(self) -> git_cache_git::FetchOptions<'static> {
        git_cache_git::FetchOptions {
            filter: self.filter,
            depth: self.depth,
            refetch: self.refetch,
            unshallow: self.unshallow,
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
            None,
            DirectFetchOptions::from_intent(intent),
        ))
        .await
    }

    async fn ensure_wants_read_through(
        &self,
        repo: &RepoKey,
        object_ids: &[CommitSha],
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
        let mut force_refetch =
            !fetch_options.blobless_fetch() && fs::try_exists(&partial_marker).await?;
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
        // A repo hydrated only by depth-limited fetches is shallow: serving a
        // full-history intent from it would stream a pack whose commit
        // parents stop at the shallow boundary while the client repo is not
        // marked shallow — a silently corrupt clone. Force the batched fetch
        // to unshallow the cache repo before serving such intents. Lazy
        // exact-oid fetches are unaffected (`fetch_objects` never
        // unshallows), so blobless checkout blob storms stay cheap.
        let needs_unshallow =
            fetch_options.depth.is_none() && fs::try_exists(repo_dir.join("shallow")).await?;
        let fetch_options = if needs_unshallow {
            info!(
                %repo,
                "cache repo is shallow; forcing unshallow fetch for full-history wants"
            );
            fetch_options.with_unshallow()
        } else {
            fetch_options
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

        for object_id in object_ids {
            if let Some(object_type) = object_types.get(object_id) {
                if object_type != "commit" {
                    non_commit_wants += 1;
                    continue;
                }

                if !force_refetch
                    && !needs_unshallow
                    && self.commit_tree_exists_no_lazy(&repo_dir, object_id).await
                {
                    self.expose_served_commit(&repo_dir, object_id).await?;
                    served_commits += 1;
                    continue;
                }
            }

            if !force_refetch && !needs_unshallow && fetch_options.hydrate_manifests {
                if let Some(manifest) = self.get_commit_manifest(repo, object_id).await? {
                    if manifest.complete {
                        match Box::pin(self.hydrate_commit_in_repo(&repo_dir, &manifest)).await {
                            Ok(()) => {
                                self.expose_served_commit(&repo_dir, object_id).await?;
                                hydrated_commits += 1;
                                continue;
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
            // A fresh clone wants the tip of every advertised ref, so missing
            // wants must be fetched in batched upstream invocations: one fetch
            // for all advertised refspecs plus one for raw SHAs, instead of one
            // subprocess per want.
            let upstream_url = self.upstream_url(repo)?;
            let upstream_git = self.upstream_git(&upstream_url)?;

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
                    Ok(DirectFetchedWantKind::Commit) => fetched_commits += 1,
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
                        Ok(DirectFetchedWantKind::Commit) => fetched_commits += 1,
                        Ok(DirectFetchedWantKind::NonCommit) => fetched_non_commit_wants += 1,
                        Err(err) => {
                            return Err(GitCacheError::NotFound(format!(
                                "object `{object_id}` could not be fetched from upstream: {err}"
                            )));
                        }
                    }
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
            match materializer.state.git.fsck(&repo_dir).await {
                Ok(_) => info!(
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
        });
    }

    /// Debounced background maintenance that keeps served repos fast: a full
    /// `git repack -a -d --write-bitmap-index` plus a commit-graph rewrite
    /// after hydration, so server-side pack-objects can reuse pack bytes and
    /// bitmaps instead of recomputing deltas over millions of objects. At
    /// most one maintenance run per repo is queued or running at a time.
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
            let result = async {
                materializer.state.git.repack_for_serving(&repo_dir).await?;
                materializer.state.git.commit_graph_write(&repo_dir).await?;
                CoreResult::Ok(())
            }
            .await;
            match result {
                Ok(()) => info!(
                    %repo,
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
        let _repo_lock = self.lock_repo(repo).await?;
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
        let full_history_on_shallow_repo =
            intent.depth.is_none() && fs::try_exists(repo_dir.join("shallow")).await?;

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
                if full_history_on_shallow_repo {
                    info!(
                        %repo,
                        commit = %object_id,
                        wants_count,
                        "direct git cache prepare declined: repo is shallow, commit want needs full history"
                    );
                    return Ok(false);
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
                    Some(comparison),
                    fetch_options,
                ))
                .await
            }
            None => {
                Box::pin(self.ensure_wants_read_through(repo, &intent.wants, None, fetch_options))
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
    let mut framed = Vec::with_capacity(refs_output.len() + 34);
    framed.extend_from_slice(b"001e# service=git-upload-pack\n0000");
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
