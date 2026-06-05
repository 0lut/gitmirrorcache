use super::*;

const SERVED_REPO_CONFIG_MARKER: &str = "git-cache-serving-config-v1";

impl Materializer {
    pub async fn compare_upstream_refs(&self, repo: &RepoKey) -> CoreResult<UpstreamRefComparison> {
        let upstream_url = self.upstream_url(repo)?;
        let ls = self
            .upstream_git(&upstream_url)?
            .ls_remote_heads(&upstream_url)
            .await?;
        let repo_dir = self.ensure_repo_dir(repo).await?;

        let mut changed: HashMap<String, String> = HashMap::new();

        for (branch, upstream_sha) in &ls.refs {
            let local_ref = format!("refs/cache/upstream/heads/{branch}");
            let local_sha = self.state.git.rev_parse(&repo_dir, &local_ref).await.ok();
            if local_sha.as_deref() != Some(upstream_sha.as_str()) {
                changed.insert(branch.clone(), upstream_sha.clone());
            }
        }

        Ok(UpstreamRefComparison {
            changed,
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Fetch only the branches that changed (from compare_upstream_refs),
    /// update both internal cache refs and public refs, and publish manifests.
    pub async fn fetch_changed_refs(
        &self,
        repo: &RepoKey,
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        if comparison.changed.is_empty() {
            return Ok(());
        }

        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let upstream_url = self.upstream_url(repo)?;

        // Validate all branch names and SHAs from network before passing to git.
        let mut validated: Vec<(BranchName, CommitSha)> = Vec::new();
        for (branch, sha) in &comparison.changed {
            let branch_name = BranchName::parse(branch.as_str())?;
            let commit = CommitSha::parse(sha.as_str())?;
            validated.push((branch_name, commit));
        }

        let refspecs: Vec<String> = validated
            .iter()
            .map(|(branch, _)| format!("+refs/heads/{branch}:refs/cache/upstream/heads/{branch}"))
            .collect();

        self.upstream_git(&upstream_url)?
            .fetch_refs(&repo_dir, &upstream_url, &refspecs)
            .await?;

        for (branch_name, expected_commit) in &validated {
            let cache_ref = format!("refs/cache/upstream/heads/{branch_name}");
            let fetched_sha = match self.state.git.rev_parse(&repo_dir, &cache_ref).await {
                Ok(sha) => sha,
                Err(_) => {
                    warn!(%repo, %branch_name, "skipping branch: ref not found after fetch (upstream may have moved)");
                    continue;
                }
            };

            if fetched_sha.as_str() != expected_commit.as_str() {
                warn!(
                    %repo, %branch_name,
                    expected = expected_commit.as_str(),
                    fetched = fetched_sha.as_str(),
                    "skipping branch: upstream moved during fetch"
                );
                continue;
            }

            let is_default_branch =
                comparison.default_branch.as_deref() == Some(branch_name.as_str());
            self.publish_generation(
                repo,
                &repo_dir,
                expected_commit,
                Some(branch_name.clone()),
                is_default_branch,
            )
            .await?;

            if !self.upstream_auth.is_authenticated() {
                self.state
                    .git
                    .update_ref(
                        &repo_dir,
                        &format!("refs/heads/{branch_name}"),
                        expected_commit.as_str(),
                    )
                    .await?;
            }
        }

        if !self.upstream_auth.is_authenticated() {
            if let Some(default_branch) = &comparison.default_branch {
                let db = BranchName::parse(default_branch.as_str())?;
                self.state
                    .git
                    .symbolic_ref(&repo_dir, "HEAD", &format!("refs/heads/{db}"))
                    .await?;
            }
        }

        info!(
            %repo,
            changed_count = validated.len(),
            "fetched and published changed refs"
        );

        Ok(())
    }

    /// Fetch the upstream ref advertisement for a repo without downloading
    /// any objects.  Returns the structured ref data so the API layer can
    /// synthesize the pkt-line response directly, avoiding the need to
    /// materialise objects just for ls-remote.
    pub async fn upstream_refs(&self, repo: &RepoKey) -> CoreResult<UpstreamRefComparison> {
        self.validate_host(repo)?;
        let upstream_url = self.upstream_url(repo)?;
        let ls = self
            .upstream_git(&upstream_url)?
            .ls_remote_heads(&upstream_url)
            .await?;

        Ok(UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Sync public refs from the current upstream advertisement without
    /// fetching (used when all branches already match).
    pub async fn sync_public_refs(
        &self,
        repo: &RepoKey,
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;

        for (branch, sha) in &comparison.all_upstream {
            let ref_name = format!("refs/heads/{branch}");
            let local = self.state.git.rev_parse(&repo_dir, &ref_name).await.ok();
            if local.as_deref() != Some(sha.as_str()) {
                self.state.git.update_ref(&repo_dir, &ref_name, sha).await?;
            }
        }

        if let Some(default_branch) = &comparison.default_branch {
            let db = BranchName::parse(default_branch.as_str())?;
            self.state
                .git
                .symbolic_ref(&repo_dir, "HEAD", &format!("refs/heads/{db}"))
                .await?;
        }

        Ok(())
    }

    /// Ensure all wanted OIDs are available locally.
    ///
    /// Authenticated requests arrive here only after the API layer has proven
    /// repo-level read access for the request. They do not need per-object
    /// upstream auth proof; this path only hydrates/fetches missing objects.
    ///
    /// Anonymous requests are different: object existence in the shared
    /// repo-local cache is not authorization. The hot path may serve only wants
    /// that are locally proven reachable from public refs. If that proof is
    /// missing, we fall back to an anonymous upstream proof/fetch or fail.
    pub async fn ensure_wants_available(&self, repo: &RepoKey, wants: &[String]) -> CoreResult<()> {
        if self.upstream_auth.is_authenticated() {
            return self.ensure_authorized_wants_available(repo, wants).await;
        }

        if !self.upstream_auth.is_authenticated()
            && self
                .ensure_cached_public_wants_available(repo, wants)
                .await?
        {
            return Ok(());
        }

        let comparison = self.compare_upstream_refs(repo).await?;
        self.ensure_wants_available_from_comparison(repo, wants, &comparison)
            .await
    }

    async fn ensure_authorized_wants_available(
        &self,
        repo: &RepoKey,
        wants: &[String],
    ) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let object_ids: Vec<CommitSha> = wants
            .iter()
            .map(|want_sha| CommitSha::parse(want_sha.as_str()))
            .collect::<CoreResult<_>>()?;
        if object_ids.is_empty() {
            return Ok(());
        }

        let upstream_url = self.upstream_url(repo)?;
        let upstream_git = self.upstream_git(&upstream_url)?;

        for object_id in object_ids {
            let _repo_lock = self.lock_repo(repo).await?;
            self.hydrate_complete_manifest_for_want(repo, &repo_dir, &object_id)
                .await?;

            let mut last_fetch_error = None;
            if !self.want_ready_for_serving(&repo_dir, &object_id).await?
                && self
                    .verify_pending_generation_for_commit(repo, &object_id)
                    .await?
            {
                self.hydrate_complete_manifest_for_want(repo, &repo_dir, &object_id)
                    .await?;
            }
            if !self.want_ready_for_serving(&repo_dir, &object_id).await? {
                last_fetch_error = upstream_git
                    .fetch_object(&repo_dir, &upstream_url, &object_id)
                    .await
                    .err();

                self.hydrate_complete_manifest_for_want(repo, &repo_dir, &object_id)
                    .await?;

                if !self.want_ready_for_serving(&repo_dir, &object_id).await? {
                    match upstream_git.fetch_all_heads(&repo_dir, &upstream_url).await {
                        Ok(_) => {}
                        Err(fetch_all_error) => {
                            if let Some(fetch_object_error) = &last_fetch_error {
                                debug!(
                                    %repo,
                                    %object_id,
                                    object_error = %fetch_object_error,
                                    all_heads_error = %fetch_all_error,
                                    "authorized direct Git fallback fetch failed"
                                );
                            }
                            last_fetch_error = Some(fetch_all_error);
                        }
                    }
                }
            }

            if !self.want_ready_for_serving(&repo_dir, &object_id).await? {
                if let Some(error) = last_fetch_error {
                    return Err(error);
                }
                return Err(GitCacheError::NotFound(format!(
                    "object `{object_id}` is authorized but unavailable locally"
                )));
            }

            if self.commit_exists(&repo_dir, &object_id).await {
                self.expose_served_commit(&repo_dir, &object_id).await?;
            }
        }

        Ok(())
    }

    async fn hydrate_complete_manifest_for_want(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        object_id: &CommitSha,
    ) -> CoreResult<()> {
        if self.want_ready_for_serving(repo_dir, object_id).await? {
            return Ok(());
        }
        if let Some(manifest) = self.get_commit_manifest(repo, object_id).await? {
            if manifest.complete {
                self.hydrate_commit_in_repo(repo_dir, &manifest).await?;
            }
        }
        Ok(())
    }

    async fn want_ready_for_serving(
        &self,
        repo_dir: &FsPath,
        object_id: &CommitSha,
    ) -> CoreResult<bool> {
        if !self.object_exists(repo_dir, object_id).await {
            return Ok(false);
        }
        if self.commit_exists(repo_dir, object_id).await {
            return Ok(self.commit_ready_for_serving(repo_dir, object_id).await);
        }
        Ok(true)
    }

    async fn ensure_cached_public_wants_available(
        &self,
        repo: &RepoKey,
        wants: &[String],
    ) -> CoreResult<bool> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let object_ids: Vec<CommitSha> = wants
            .iter()
            .map(|want_sha| CommitSha::parse(want_sha.as_str()))
            .collect::<CoreResult<_>>()?;
        if object_ids.is_empty() {
            return Ok(true);
        }

        let _repo_lock = self.lock_repo(repo).await?;
        let public_tips = self
            .state
            .git
            .for_each_ref_commits(&repo_dir, "refs/heads")
            .await?;
        if public_tips.is_empty() {
            return Ok(false);
        }

        let object_types = self
            .state
            .git
            .cat_file_batch_types(&repo_dir, &object_ids)
            .await?;

        for object_id in object_ids {
            let Some(object_type) = object_types.get(&object_id) else {
                return Ok(false);
            };

            if object_type == "commit" {
                if !self.commit_ready_for_serving(&repo_dir, &object_id).await {
                    return Ok(false);
                }
                if public_tips.iter().any(|tip| tip == &object_id)
                    || self
                        .state
                        .git
                        .object_reachable_from_commits(&repo_dir, &object_id, &public_tips)
                        .await?
                {
                    self.expose_served_commit(&repo_dir, &object_id).await?;
                    continue;
                }

                return Ok(false);
            }

            if !self
                .state
                .git
                .object_reachable_from_commits(&repo_dir, &object_id, &public_tips)
                .await?
            {
                return Ok(false);
            }
        }

        Ok(true)
    }

    pub(super) async fn ensure_wants_available_from_comparison(
        &self,
        repo: &RepoKey,
        wants: &[String],
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;

        let authorized_tips: HashSet<String> = comparison
            .all_upstream
            .values()
            .map(|sha| sha.to_ascii_lowercase())
            .collect();
        let upstream_tips: Vec<CommitSha> = comparison
            .all_upstream
            .values()
            .map(|sha| CommitSha::parse(sha.as_str()))
            .collect::<CoreResult<_>>()?;
        let upstream_url = self.upstream_url(repo)?;
        let upstream_git = self.upstream_git(&upstream_url)?;
        let object_ids: Vec<CommitSha> = wants
            .iter()
            .map(|want_sha| CommitSha::parse(want_sha.as_str()))
            .collect::<CoreResult<_>>()?;

        for object_id in object_ids {
            let proven_by_advertisement = authorized_tips.contains(object_id.as_str());
            if !proven_by_advertisement {
                if !self.state.config.git_remote.commit_read_through {
                    return Err(GitCacheError::Forbidden(format!(
                        "object `{object_id}` is not authorized by the current upstream advertisement"
                    )));
                }

                {
                    let _repo_lock = self.lock_repo(repo).await?;
                    let existed_before_fetch = self.object_exists(&repo_dir, &object_id).await;
                    match upstream_git
                        .fetch_object(&repo_dir, &upstream_url, &object_id)
                        .await
                    {
                        Ok(_) if !existed_before_fetch => {}
                        Ok(_) => {
                            upstream_git
                                .fetch_all_heads(&repo_dir, &upstream_url)
                                .await?;
                            if !self
                                .object_reachable_from_upstream_tips(
                                    &repo_dir,
                                    &object_id,
                                    &upstream_tips,
                                )
                                .await?
                            {
                                return self.unproven_want_error(&object_id, None);
                            }
                        }
                        Err(err) => {
                            upstream_git
                                .fetch_all_heads(&repo_dir, &upstream_url)
                                .await?;
                            if !self
                                .object_reachable_from_upstream_tips(
                                    &repo_dir,
                                    &object_id,
                                    &upstream_tips,
                                )
                                .await?
                            {
                                return self.unproven_want_error(&object_id, Some(&err));
                            }
                        }
                    }
                }
            } else if self.get_commit_manifest(repo, &object_id).await?.is_none()
                && (!self.object_exists(&repo_dir, &object_id).await
                    || (self.commit_exists(&repo_dir, &object_id).await
                        && !self.commit_ready_for_serving(&repo_dir, &object_id).await))
                && !self
                    .verify_pending_generation_for_commit(repo, &object_id)
                    .await?
            {
                self.fetch_refs_for_advertised_want(repo, comparison, &object_id)
                    .await?;
            }

            let _repo_lock = self.lock_repo(repo).await?;
            if !self.object_exists(&repo_dir, &object_id).await
                || (self.commit_exists(&repo_dir, &object_id).await
                    && !self.commit_ready_for_serving(&repo_dir, &object_id).await)
            {
                if let Some(manifest) = self.get_commit_manifest(repo, &object_id).await? {
                    if manifest.complete {
                        self.hydrate_commit_in_repo(&repo_dir, &manifest).await?;
                    }
                }
            }

            if !self.object_exists(&repo_dir, &object_id).await {
                return Err(GitCacheError::NotFound(format!(
                    "object `{object_id}` is authorized but unavailable locally"
                )));
            }

            let object_types = self
                .state
                .git
                .cat_file_batch_types(&repo_dir, std::slice::from_ref(&object_id))
                .await?;
            let Some(object_type) = object_types.get(&object_id) else {
                return Err(GitCacheError::NotFound(format!(
                    "object `{object_id}` is authorized but unavailable locally"
                )));
            };
            if object_type != "commit" {
                continue;
            }

            if !self.commit_ready_for_serving(&repo_dir, &object_id).await {
                return Err(GitCacheError::NotFound(format!(
                    "commit `{object_id}` is incomplete after upstream fetch"
                )));
            }

            self.expose_served_commit(&repo_dir, &object_id).await?;

            // Create a ref so that `bundle create --all` has something
            // to include.  We use refs/cache/ which is hidden from
            // clients by configure_served_repo.
            let cache_ref = format!("refs/cache/commits/{object_id}");
            self.state
                .git
                .update_ref(&repo_dir, &cache_ref, object_id.as_str())
                .await?;

            if self.get_commit_manifest(repo, &object_id).await?.is_none() {
                self.publish_generation(repo, &repo_dir, &object_id, None, false)
                    .await?;
            }
        }

        Ok(())
    }

    async fn fetch_refs_for_advertised_want(
        &self,
        repo: &RepoKey,
        comparison: &UpstreamRefComparison,
        object_id: &CommitSha,
    ) -> CoreResult<()> {
        let changed = comparison
            .all_upstream
            .iter()
            .filter(|(_, sha)| sha.eq_ignore_ascii_case(object_id.as_str()))
            .map(|(branch, sha)| (branch.clone(), sha.clone()))
            .collect::<HashMap<_, _>>();
        if changed.is_empty() {
            return Ok(());
        }

        let wanted_comparison = UpstreamRefComparison {
            changed,
            default_branch: comparison.default_branch.clone(),
            all_upstream: comparison.all_upstream.clone(),
        };
        self.fetch_changed_refs(repo, &wanted_comparison).await
    }

    pub(super) async fn object_reachable_from_upstream_tips(
        &self,
        repo_dir: &FsPath,
        object_id: &CommitSha,
        upstream_tips: &[CommitSha],
    ) -> CoreResult<bool> {
        Ok(self.object_exists(repo_dir, object_id).await
            && self
                .state
                .git
                .object_reachable_from_commits(repo_dir, object_id, upstream_tips)
                .await?)
    }

    pub(super) fn unproven_want_error(
        &self,
        object_id: &CommitSha,
        upstream_error: Option<&GitCacheError>,
    ) -> CoreResult<()> {
        let suffix = upstream_error
            .map(|err| format!(": {err}"))
            .unwrap_or_default();
        if self.upstream_auth.is_authenticated() {
            Err(GitCacheError::Forbidden(format!(
                "object `{object_id}` is not authorized by upstream{suffix}"
            )))
        } else {
            Err(GitCacheError::NotFound(format!(
                "object `{object_id}` was not available from upstream{suffix}"
            )))
        }
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
    ) -> CoreResult<UploadPackProcess> {
        let wants = parse_want_lines(body);
        if !wants.is_empty() {
            self.ensure_wants_available(repo, &wants).await?;
        }
        let repo_dir = self.ensure_repo_dir(repo).await?;
        self.configure_served_repo(&repo_dir).await?;
        self.state
            .git
            .upload_pack_spawn(&repo_dir, body.clone())
            .await
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamRefComparison {
    pub changed: HashMap<String, String>,
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
    synthesize_ref_advertisement_inner(comparison, true)
}

pub fn synthesize_protected_ref_advertisement(comparison: &UpstreamRefComparison) -> Vec<u8> {
    synthesize_ref_advertisement_inner(comparison, false)
}

fn synthesize_ref_advertisement_inner(
    comparison: &UpstreamRefComparison,
    advertise_expanded_wants: bool,
) -> Vec<u8> {
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

    let expanded_want_caps = if advertise_expanded_wants {
        " filter allow-tip-sha1-in-want allow-reachable-sha1-in-want"
    } else {
        ""
    };
    let caps = format!(
        "multi_ack thin-pack side-band side-band-64k ofs-delta \
         shallow deepen-since deepen-not deepen-relative no-progress \
         include-tag multi_ack_detailed no-done{expanded_want_caps}{symref} \
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
