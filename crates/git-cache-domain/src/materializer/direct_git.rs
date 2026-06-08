use super::*;

const SERVED_REPO_CONFIG_MARKER: &str = "git-cache-serving-config-v1";
const BLOBLESS_FETCH_FILTER: &str = "blob:none";

#[cfg(test)]
const DIRECT_GENERATION_PUBLISH_DELAY: StdDuration = StdDuration::from_millis(20);
#[cfg(not(test))]
const DIRECT_GENERATION_PUBLISH_DELAY: StdDuration = StdDuration::from_secs(30);

enum DirectFetchedWantKind {
    Commit,
    NonCommit,
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
    pub async fn ensure_wants_available(&self, repo: &RepoKey, wants: &[String]) -> CoreResult<()> {
        Box::pin(self.ensure_wants_read_through(repo, wants, false)).await
    }

    pub(super) async fn ensure_wants_available_from_comparison(
        &self,
        repo: &RepoKey,
        wants: &[String],
        _comparison: &UpstreamRefComparison,
        blobless_fetch: bool,
    ) -> CoreResult<()> {
        Box::pin(self.ensure_wants_read_through(repo, wants, blobless_fetch)).await
    }

    async fn ensure_wants_read_through(
        &self,
        repo: &RepoKey,
        wants: &[String],
        blobless_fetch: bool,
    ) -> CoreResult<()> {
        let started = Instant::now();
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let object_ids: Vec<CommitSha> = wants
            .iter()
            .map(|want_sha| CommitSha::parse(want_sha.as_str()))
            .collect::<CoreResult<_>>()?;
        let object_count = object_ids.len();
        let object_types = self
            .state
            .git
            .cat_file_batch_types(&repo_dir, &object_ids)
            .await?;
        let mut non_commit_wants = 0usize;
        let mut served_commits = 0usize;
        let mut hydrated_commits = 0usize;
        let mut fetched_commits = 0usize;
        let mut fetched_non_commit_wants = 0usize;
        let fetch_filter = blobless_fetch.then_some(BLOBLESS_FETCH_FILTER);

        for object_id in object_ids {
            if let Some(object_type) = object_types.get(&object_id) {
                if object_type != "commit" {
                    non_commit_wants += 1;
                    continue;
                }

                if self.commit_tree_exists(&repo_dir, &object_id).await {
                    self.expose_served_commit(&repo_dir, &object_id).await?;
                    served_commits += 1;
                    continue;
                }
            }

            if let Some(manifest) = self.get_commit_manifest(repo, &object_id).await? {
                if manifest.complete {
                    Box::pin(self.hydrate_commit_in_repo(&repo_dir, &manifest)).await?;
                    self.expose_served_commit(&repo_dir, &object_id).await?;
                    hydrated_commits += 1;
                    continue;
                }
            }

            if !self.state.config.git_remote.commit_read_through {
                return Err(GitCacheError::NotFound(format!(
                    "commit `{object_id}` is not available and read-through is disabled"
                )));
            }

            info!(%repo, commit = %object_id, "direct git read-through fetch for wanted commit");
            let upstream_url = self.upstream_url(repo)?;
            let upstream_git = self.upstream_git(&upstream_url)?;
            let fetch_result = upstream_git
                .fetch_object_with_filter(&repo_dir, &upstream_url, &object_id, fetch_filter)
                .await;

            if let Err(fetch_err) = fetch_result {
                upstream_git
                    .fetch_all_heads_with_filter(&repo_dir, &upstream_url, fetch_filter)
                    .await?;
                match self
                    .prepare_fetched_direct_want(repo, &repo_dir, &object_id)
                    .await
                {
                    Ok(DirectFetchedWantKind::Commit) => fetched_commits += 1,
                    Ok(DirectFetchedWantKind::NonCommit) => fetched_non_commit_wants += 1,
                    Err(_) => {
                        return Err(GitCacheError::NotFound(format!(
                            "object `{object_id}` could not be fetched from upstream: {fetch_err}"
                        )));
                    }
                }
                continue;
            }

            match self
                .prepare_fetched_direct_want(repo, &repo_dir, &object_id)
                .await?
            {
                DirectFetchedWantKind::Commit => fetched_commits += 1,
                DirectFetchedWantKind::NonCommit => fetched_non_commit_wants += 1,
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
            blobless_fetch,
            elapsed_ms = elapsed_ms(started),
            "ensured direct git wants via read-through"
        );
        Ok(())
    }

    async fn prepare_fetched_direct_want(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        object_id: &CommitSha,
    ) -> CoreResult<DirectFetchedWantKind> {
        let object_types = self
            .state
            .git
            .cat_file_batch_types(repo_dir, std::slice::from_ref(object_id))
            .await?;
        let Some(object_type) = object_types.get(object_id).map(String::as_str) else {
            return Err(GitCacheError::NotFound(format!(
                "object `{object_id}` not found after upstream fetch"
            )));
        };

        if object_type != "commit" {
            return Ok(DirectFetchedWantKind::NonCommit);
        }

        if !self.commit_ready_for_serving(repo_dir, object_id).await {
            return Err(GitCacheError::NotFound(format!(
                "commit `{object_id}` not found or incomplete after upstream fetch"
            )));
        }
        self.expose_served_commit(repo_dir, object_id).await?;

        // Keep a hidden ref so async generation publication has a stable tip
        // to bundle. The ref is hidden from clients by configure_served_repo.
        let cache_ref = format!("refs/cache/commits/{object_id}");
        self.state
            .git
            .update_ref(repo_dir, &cache_ref, object_id.as_str())
            .await?;

        self.enqueue_direct_generation_publish(
            repo.clone(),
            repo_dir.to_path_buf(),
            object_id.clone(),
        );
        Ok(DirectFetchedWantKind::Commit)
    }

    fn enqueue_direct_generation_publish(
        &self,
        repo: RepoKey,
        repo_dir: PathBuf,
        commit: CommitSha,
    ) {
        let materializer = self.clone();
        info!(
            %repo,
            %commit,
            delay_ms = DIRECT_GENERATION_PUBLISH_DELAY.as_millis(),
            "queued direct git background generation publish"
        );
        tokio::spawn(async move {
            tokio::time::sleep(DIRECT_GENERATION_PUBLISH_DELAY).await;
            let started = Instant::now();
            match materializer
                .publish_generation(&repo, &repo_dir, &commit, None, false)
                .await
            {
                Ok(generation) => info!(
                    %repo,
                    %commit,
                    %generation,
                    elapsed_ms = elapsed_ms(started),
                    "direct git background generation publish finished"
                ),
                Err(err) => warn!(
                    %repo,
                    %commit,
                    %err,
                    elapsed_ms = elapsed_ms(started),
                    "direct git background generation publish failed"
                ),
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
    pub async fn configure_served_repo(&self, repo_dir: &FsPath) -> CoreResult<()> {
        let marker = repo_dir.join(SERVED_REPO_CONFIG_MARKER);
        if fs::try_exists(&marker).await? {
            return Ok(());
        }

        self.state
            .git
            .set_config(repo_dir, "uploadpack.allowAnySHA1InWant", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "uploadpack.allowFilter", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "uploadpack.allowReachableSHA1InWant", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "uploadpack.hideRefs", "refs/cache")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "transfer.hideRefs", "refs/cache")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "pack.useBitmap", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "repack.writeBitmaps", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "pack.compression", "1")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "core.compression", "1")
            .await?;
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
        let wants = parse_want_lines(body);
        let blobless_fetch = upload_pack_requests_blobless_filter(body);
        info!(
            %repo,
            wants_count = wants.len(),
            cached_ref_proof = comparison.is_some(),
            blobless_fetch,
            "direct git upload-pack preparation started"
        );
        if !wants.is_empty() {
            let ensure_started = Instant::now();
            match comparison {
                Some(comparison) => {
                    Box::pin(self.ensure_wants_available_from_comparison(
                        repo,
                        &wants,
                        comparison,
                        blobless_fetch,
                    ))
                    .await?;
                }
                None => {
                    Box::pin(self.ensure_wants_read_through(repo, &wants, blobless_fetch)).await?
                }
            }
            info!(
                %repo,
                wants_count = wants.len(),
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

pub async fn advertise_refs(state: &AppState, repo: &FsPath) -> CoreResult<Vec<u8>> {
    Ok(state
        .git
        .upload_pack_advertise_refs(repo, state.config.max_git_output_bytes)
        .await?
        .stdout)
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

pub async fn upload_pack(state: &AppState, repo: &FsPath, body: Bytes) -> CoreResult<Vec<u8>> {
    Ok(state
        .git
        .upload_pack_stateless_rpc(
            repo,
            &body,
            state.config.max_git_output_bytes,
            state.config.max_git_output_bytes,
        )
        .await?
        .stdout)
}

/// Frame a ref advertisement with the Git smart-HTTP service header.
pub fn frame_ref_advertisement(refs_output: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(refs_output.len() + 34);
    framed.extend_from_slice(b"001e# service=git-upload-pack\n0000");
    framed.extend_from_slice(refs_output);
    framed
}

/// Parse `want <oid>` lines from a Git pkt-line formatted upload-pack request.
pub fn parse_want_lines(body: &[u8]) -> Vec<String> {
    let mut wants = Vec::new();
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
            let line_str = line_str.trim();
            if let Some(rest) = line_str.strip_prefix("want ") {
                let oid = rest.split_whitespace().next().unwrap_or("");
                if !oid.is_empty() {
                    wants.push(oid.to_string());
                }
            }
        }

        offset += pkt_len;
    }
    wants
}

pub fn upload_pack_requests_blobless_filter(body: &[u8]) -> bool {
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
            if line_str.trim() == "filter blob:none" {
                return true;
            }
        }

        offset += pkt_len;
    }
    false
}
