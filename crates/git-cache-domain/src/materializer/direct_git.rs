use super::*;

const SERVED_REPO_CONFIG_MARKER: &str = "git-cache-serving-config-v1";

impl Materializer {
    pub fn direct_repo_available(&self, repo: &RepoKey) -> bool {
        self.repo_dir(repo).join("config").exists()
    }

    pub async fn compare_upstream_refs(&self, repo: &RepoKey) -> CoreResult<UpstreamRefComparison> {
        let started = Instant::now();
        let upstream_url = self.upstream_url(repo)?;
        let ls_started = Instant::now();
        let ls = self
            .upstream_git(&upstream_url)?
            .ls_remote_heads(&upstream_url)
            .await?;
        let ls_elapsed_ms = elapsed_ms(ls_started);
        let repo_dir = self.ensure_repo_dir(repo).await?;

        let mut changed: HashMap<String, String> = HashMap::new();

        for (branch, upstream_sha) in &ls.refs {
            let local_ref = format!("refs/cache/upstream/heads/{branch}");
            let local_sha = self.state.git.rev_parse(&repo_dir, &local_ref).await.ok();
            if local_sha.as_deref() != Some(upstream_sha.as_str()) {
                changed.insert(branch.clone(), upstream_sha.clone());
            }
        }

        info!(
            %repo,
            refs_count = ls.refs.len(),
            changed_count = changed.len(),
            default_branch = ls.default_branch.as_deref().unwrap_or("<none>"),
            ls_remote_elapsed_ms = ls_elapsed_ms,
            elapsed_ms = elapsed_ms(started),
            "compared upstream refs"
        );
        Ok(UpstreamRefComparison {
            changed,
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Fetch only the branches that changed (from compare_upstream_refs) and
    /// update serving refs.
    ///
    /// This is retained for explicit warming/background maintenance work.
    /// Direct Git upload-pack POSTs must not call it: clone/fetch requests
    /// should prove access, check local availability, and fail fast on cache
    /// misses instead of importing pack data during the client request.
    pub async fn fetch_changed_refs(
        &self,
        repo: &RepoKey,
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        let started = Instant::now();
        if comparison.changed.is_empty() {
            info!(%repo, "direct git changed-ref fetch skipped: no changed refs");
            return Ok(());
        }

        let upstream_url = self.upstream_url(repo)?;
        let repo_dir = self.ensure_repo_dir(repo).await?;

        // Validate all branch names and SHAs from network before passing to git.
        let mut validated: Vec<(BranchName, CommitSha)> = Vec::new();
        for (branch, sha) in &comparison.changed {
            let branch_name = BranchName::parse(branch.as_str())?;
            let commit = CommitSha::parse(sha.as_str())?;
            validated.push((branch_name, commit));
        }

        if !self.upstream_auth.is_authenticated() {
            let restore_started = Instant::now();
            let mut restored_count = 0usize;
            for (branch, _) in &validated {
                if let Some(commit) = Box::pin(
                    self.restore_hot_upstream_ref_base_from_manifest(repo, &repo_dir, branch),
                )
                .await?
                {
                    restored_count += 1;
                    debug!(
                        %repo,
                        %branch,
                        %commit,
                        "restored public ref manifest as hidden fetch negotiation base"
                    );
                }
            }
            info!(
                %repo,
                changed_count = validated.len(),
                restored_count,
                elapsed_ms = elapsed_ms(restore_started),
                "checked public ref manifests for direct git fetch negotiation"
            );
        }

        let refspecs: Vec<String> = validated
            .iter()
            .map(|(branch, _)| format!("+refs/heads/{branch}:refs/cache/upstream/heads/{branch}"))
            .collect();

        let _repo_lock = self.lock_repo(repo).await?;
        let fetch_started = Instant::now();
        self.upstream_git(&upstream_url)?
            .fetch_refs(&repo_dir, &upstream_url, &refspecs)
            .await?;
        info!(
            %repo,
            changed_count = validated.len(),
            elapsed_ms = elapsed_ms(fetch_started),
            "fetched changed refs from upstream"
        );

        let publish_started = Instant::now();
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
            publish_elapsed_ms = elapsed_ms(publish_started),
            elapsed_ms = elapsed_ms(started),
            "fetched and published changed refs"
        );

        Ok(())
    }

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
            changed: HashMap::new(),
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Convert an upstream access proof into the ref advertisement this cache
    /// can actually serve without fetching.
    ///
    /// Direct Git GETs use upstream refs to prove that the repository is
    /// reachable for the request, but the advertised commits must be local
    /// and ready. Otherwise a fresh upstream tip can be advertised and then
    /// rejected by the following upload-pack POST.
    pub async fn locally_available_refs(
        &self,
        repo: &RepoKey,
        upstream: &UpstreamRefComparison,
    ) -> CoreResult<UpstreamRefComparison> {
        let started = Instant::now();
        let repo_dir = self.repo_dir(repo);
        if !repo_dir.join("config").exists() {
            return Err(Self::direct_git_repo_cache_miss(repo));
        }

        let upstream_branches: HashSet<&str> =
            upstream.all_upstream.keys().map(String::as_str).collect();
        let mut refs = HashMap::new();

        for ref_prefix in ["refs/heads", "refs/cache/upstream/heads"] {
            for (ref_name, commit) in self.state.git.for_each_ref(&repo_dir, ref_prefix).await? {
                let Some(branch) = ref_name
                    .strip_prefix("refs/heads/")
                    .or_else(|| ref_name.strip_prefix("refs/cache/upstream/heads/"))
                else {
                    continue;
                };
                if !upstream_branches.contains(branch) || refs.contains_key(branch) {
                    continue;
                }
                if self.commit_ready_for_serving(&repo_dir, &commit).await {
                    refs.insert(branch.to_string(), commit.to_string());
                }
            }
        }

        if refs.is_empty() {
            return Err(GitCacheError::UpstreamUnavailable(format!(
                "repo `{repo}` has no locally available refs"
            )));
        }

        let default_branch = upstream
            .default_branch
            .as_ref()
            .filter(|branch| refs.contains_key(branch.as_str()))
            .cloned();

        info!(
            %repo,
            upstream_refs_count = upstream.all_upstream.len(),
            advertised_refs_count = refs.len(),
            default_branch = default_branch.as_deref().unwrap_or("<none>"),
            elapsed_ms = elapsed_ms(started),
            "selected locally available refs for direct git advertisement"
        );

        Ok(UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch,
            all_upstream: refs,
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
        Box::pin(self.ensure_wants_read_through(repo, wants)).await
    }

    pub(super) async fn ensure_wants_available_from_comparison(
        &self,
        repo: &RepoKey,
        wants: &[String],
        _comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        Box::pin(self.ensure_wants_read_through(repo, wants)).await
    }

    async fn ensure_wants_read_through(&self, repo: &RepoKey, wants: &[String]) -> CoreResult<()> {
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
                .fetch_object(&repo_dir, &upstream_url, &object_id)
                .await;

            if let Err(fetch_err) = fetch_result {
                upstream_git
                    .fetch_all_heads(&repo_dir, &upstream_url)
                    .await?;
                if self.object_exists(&repo_dir, &object_id).await {
                    if self.commit_ready_for_serving(&repo_dir, &object_id).await {
                        self.expose_served_commit(&repo_dir, &object_id).await?;
                        fetched_commits += 1;
                    } else if self.commit_exists(&repo_dir, &object_id).await {
                        return Err(GitCacheError::NotFound(format!(
                            "commit `{object_id}` is incomplete after upstream fetch"
                        )));
                    }
                    continue;
                }
                return Err(GitCacheError::NotFound(format!(
                    "object `{object_id}` could not be fetched from upstream: {fetch_err}"
                )));
            }

            if !self.commit_ready_for_serving(&repo_dir, &object_id).await {
                return Err(GitCacheError::NotFound(format!(
                    "commit `{object_id}` not found or incomplete after upstream fetch"
                )));
            }
            self.expose_served_commit(&repo_dir, &object_id).await?;
            fetched_commits += 1;

            // Create a ref so that `bundle create --all` has something
            // to include.  We use refs/cache/ which is hidden from
            // clients by configure_served_repo.
            let cache_ref = format!("refs/cache/commits/{object_id}");
            self.state
                .git
                .update_ref(&repo_dir, &cache_ref, object_id.as_str())
                .await?;

            self.publish_generation(repo, &repo_dir, &object_id, None, false)
                .await?;
        }

        info!(
            %repo,
            wants_count = object_count,
            non_commit_wants,
            served_commits,
            hydrated_commits,
            fetched_commits,
            elapsed_ms = elapsed_ms(started),
            "ensured direct git wants via read-through"
        );
        Ok(())
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
        info!(
            %repo,
            wants_count = wants.len(),
            cached_ref_proof = comparison.is_some(),
            "direct git upload-pack preparation started"
        );
        if !wants.is_empty() {
            let ensure_started = Instant::now();
            match comparison {
                Some(comparison) => {
                    Box::pin(self.ensure_wants_available_from_comparison(repo, &wants, comparison))
                        .await?;
                }
                None => Box::pin(self.ensure_wants_available(repo, &wants)).await?,
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
