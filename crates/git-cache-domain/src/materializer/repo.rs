use super::*;

impl Materializer {
    pub(super) fn upstream_git(&self, remote_url: &str) -> CoreResult<git_cache_git::Git> {
        self.state
            .git
            .with_upstream_auth(remote_url, &self.upstream_auth)
    }

    pub async fn ensure_repo_dir(&self, repo: &RepoKey) -> CoreResult<PathBuf> {
        let repo_dir = self.repo_dir(repo);
        if !repo_dir.join("config").exists() {
            let _mutation_lock = self.lock_repo_mutation(repo).await?;
            if repo_dir.join("config").exists() {
                self.touch_repo_access(repo).await?;
                return Ok(repo_dir);
            }
            // Only a leftover partial directory needs invalidation; a wholly
            // absent repo dir must not go through invalidate_repo, which
            // conflicts with a repo lock the caller may already hold (e.g.
            // compaction on a cold cache).
            if repo_dir.exists() {
                self.reset_invalid_repo_cache(repo).await?;
            }
            if let Some(parent) = repo_dir.parent() {
                fs::create_dir_all(parent).await?;
            }
            self.state.git.init_bare(&repo_dir).await?;
            self.record_repo_access(repo).await?;
        } else {
            self.touch_repo_access(repo).await?;
        }
        Ok(repo_dir)
    }

    pub(super) async fn commit_exists(&self, repo_dir: &FsPath, commit: &CommitSha) -> bool {
        self.state
            .git
            .run(
                Some(repo_dir),
                ["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())],
            )
            .await
            .is_ok()
    }

    pub(super) async fn commit_tree_exists(&self, repo_dir: &FsPath, commit: &CommitSha) -> bool {
        self.state
            .git
            .run(
                Some(repo_dir),
                ["cat-file", "-e", &format!("{}^{{tree}}", commit.as_str())],
            )
            .await
            .is_ok()
    }

    pub(super) async fn commit_tree_exists_no_lazy(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> bool {
        self.state
            .git
            .run_no_lazy(
                Some(repo_dir),
                ["cat-file", "-e", &format!("{}^{{tree}}", commit.as_str())],
            )
            .await
            .is_ok()
    }

    pub(super) async fn commit_ready_for_serving(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> bool {
        self.commit_exists(repo_dir, commit).await
            && self.commit_tree_exists(repo_dir, commit).await
    }

    pub(super) async fn commit_exists_no_lazy(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> bool {
        self.state
            .git
            .run_no_lazy(
                Some(repo_dir),
                ["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())],
            )
            .await
            .is_ok()
    }

    /// Whether every want's own snapshot (commit + full tree + blobs) is
    /// locally complete, i.e. a `--depth 1` pack for these tips can be
    /// served without contacting upstream even from a partially hydrated
    /// (blobless-marked) repo.
    pub(super) async fn depth1_snapshots_complete_no_lazy(
        &self,
        repo_dir: &FsPath,
        wants: &[CommitSha],
    ) -> CoreResult<bool> {
        for want in wants {
            if !self
                .state
                .git
                .commit_snapshot_complete_no_lazy(repo_dir, want)
                .await?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a depth-limited pack for `commit` can be served from the local
    /// cache without lazy promisor fetches. This proves that Git can walk the
    /// requested finite ancestry window and that every commit in that window
    /// has its tree locally available.
    pub(super) async fn depth_window_ready_for_serving_no_lazy(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
        depth: u32,
    ) -> CoreResult<bool> {
        if depth == 0 {
            return Ok(false);
        }
        let window = self
            .state
            .git
            .commit_ancestry_window_no_lazy(repo_dir, commit, depth)
            .await?;
        if window.is_empty() {
            return Ok(false);
        }
        let shallow_commits = read_shallow_commits(repo_dir).await?;
        if !shallow_commits.is_empty() {
            if let Some(boundary_depth) = self
                .nearest_shallow_boundary_depth_no_lazy(repo_dir, commit, &shallow_commits)
                .await?
            {
                if boundary_depth.saturating_add(1) < depth {
                    return Ok(false);
                }
            }
        }
        for ancestor in &window {
            if !self.commit_tree_exists_no_lazy(repo_dir, ancestor).await {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn nearest_shallow_boundary_depth_no_lazy(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
        shallow_commits: &HashSet<String>,
    ) -> CoreResult<Option<u32>> {
        let output = match self
            .state
            .git
            .run_no_lazy(Some(repo_dir), ["rev-list", "--parents", commit.as_str()])
            .await
        {
            Ok(output) => output,
            Err(_) => return Ok(None),
        };
        let text = output.stdout_utf8("rev-list --parents")?;
        let mut parents_by_commit: HashMap<CommitSha, Vec<CommitSha>> = HashMap::new();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let mut parts = line.split_whitespace();
            let Some(commit) = parts.next() else {
                continue;
            };
            let commit = CommitSha::parse(commit)?;
            let parents = parts
                .map(CommitSha::parse)
                .collect::<CoreResult<Vec<_>>>()?;
            parents_by_commit.insert(commit, parents);
        }

        let mut queue = VecDeque::from([(commit.clone(), 0u32)]);
        let mut seen = HashSet::new();
        while let Some((current, depth)) = queue.pop_front() {
            if !seen.insert(current.clone()) {
                continue;
            }
            if shallow_commits.contains(current.as_str()) {
                return Ok(Some(depth));
            }
            if let Some(parents) = parents_by_commit.get(&current) {
                for parent in parents {
                    queue.push_back((parent.clone(), depth.saturating_add(1)));
                }
            }
        }
        Ok(None)
    }

    pub(super) async fn commit_history_complete_no_lazy(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> CoreResult<bool> {
        self.state
            .git
            .commit_history_complete_no_lazy(repo_dir, commit)
            .await
    }

    /// Whether the cache already holds enough ancestry below every client
    /// shallow `boundary` commit to serve a `--deepen=depth` request without
    /// fetching from upstream.
    ///
    /// A stateless smart-HTTP deepen runs several upload-pack negotiation
    /// rounds for one client `fetch --deepen=N`; without this guard each round
    /// would re-run `git fetch --deepen=N` and compound the cache's boundary
    /// (depth N, then 2N, ...), eventually unshallowing it. The deepen is
    /// already satisfied for a boundary commit when none of its `depth` nearest
    /// ancestors (the commits a `--deepen=depth` would reveal) coincide with
    /// the cache's own shallow graft boundary — i.e. the cache holds those
    /// commits' parents rather than cutting the history short. A boundary
    /// commit the cache does not have at all is never satisfied. Never triggers
    /// lazy promisor fetches.
    pub(super) async fn deepen_boundary_satisfied(
        &self,
        repo_dir: &FsPath,
        boundaries: &[CommitSha],
        depth: u32,
    ) -> CoreResult<bool> {
        // No declared client boundary: cannot prove coverage, so deepen.
        if boundaries.is_empty() {
            return Ok(false);
        }
        let shallow_commits = read_shallow_commits(repo_dir).await?;
        // A non-shallow cache holds full history and satisfies any deepen.
        if shallow_commits.is_empty() {
            return Ok(true);
        }
        for boundary in boundaries {
            if !self.commit_exists_no_lazy(repo_dir, boundary).await {
                return Ok(false);
            }
            let window = self
                .state
                .git
                .commit_ancestry_window_no_lazy(repo_dir, boundary, depth)
                .await?;
            // `boundary` is present (checked above) and `depth >= 1`, so a
            // healthy walk returns at least `[boundary]`. An empty window means
            // `rev-list` itself failed — per the helper's contract, treat that
            // as "cannot prove coverage" and deepen rather than fall through to
            // the all-clear below.
            if window.is_empty() {
                return Ok(false);
            }
            // The window holds `boundary` and its depth-1 nearest ancestors.
            // If any sits on the cache's shallow boundary, the cache cuts the
            // history inside the requested depth and must deepen further.
            if window
                .iter()
                .any(|commit| shallow_commits.contains(commit.as_str()))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) async fn commit_ready_for_serving_no_lazy(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> bool {
        self.commit_exists_no_lazy(repo_dir, commit).await
            && self.commit_tree_exists_no_lazy(repo_dir, commit).await
    }

    #[cfg(test)]
    pub(super) async fn object_exists(&self, repo_dir: &FsPath, object_id: &CommitSha) -> bool {
        self.state
            .git
            .run(Some(repo_dir), ["cat-file", "-e", object_id.as_str()])
            .await
            .is_ok()
    }

    pub(super) async fn expose_served_commit(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> CoreResult<()> {
        let served_ref = format!("refs/git-cache-served/commits/{commit}");
        self.state
            .git
            .update_ref(repo_dir, &served_ref, commit.as_str())
            .await?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn restore_upstream_ref_base_from_manifest(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        branch: &BranchName,
    ) -> CoreResult<Option<CommitSha>> {
        let ref_name = branch.ref_name();
        let Some(ref_manifest) = self.manifests().ref_manifest(repo, &ref_name).await? else {
            return Ok(None);
        };
        Box::pin(self.restore_ref_manifest_in_repo(
            repo,
            repo_dir,
            branch,
            &ref_manifest,
            false,
            false,
        ))
        .await?;
        Ok(Some(ref_manifest.commit))
    }

    #[cfg(test)]
    pub(super) async fn restore_ref_manifest_in_repo(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        branch: &BranchName,
        ref_manifest: &RefManifest,
        publish_public_ref: bool,
        set_public_head: bool,
    ) -> CoreResult<()> {
        let existing_commit_manifest = self
            .get_commit_manifest(repo, &ref_manifest.commit)
            .await?
            .filter(|manifest| manifest.complete);
        let commit_manifest = existing_commit_manifest
            .clone()
            .unwrap_or_else(|| CommitManifest {
                repo: repo.clone(),
                commit: ref_manifest.commit.clone(),
                generation: ref_manifest.generation,
                complete: true,
                verified_at: ref_manifest.verified_at,
            });

        // A matching ref manifest is the durable public proof; if the commit
        // is already complete locally, do not hydrate the generation bundle
        // again on hot anonymous direct-Git paths.
        if self
            .commit_ready_for_serving(repo_dir, &ref_manifest.commit)
            .await
        {
            if existing_commit_manifest.is_none() {
                self.manifests().write_commit(&commit_manifest).await?;
            }
            let _repo_lock = self.lock_repo(repo).await?;
            self.apply_restored_ref_manifest_refs(
                repo_dir,
                branch,
                ref_manifest,
                publish_public_ref,
                set_public_head,
            )
            .await?;
            return Ok(());
        }

        let _repo_lock = self.lock_repo(repo).await?;
        let public_refs_before = self
            .state
            .git
            .for_each_ref(repo_dir, "refs/heads")
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>();
        Box::pin(self.hydrate_commit_in_repo(repo_dir, &commit_manifest)).await?;
        if !self
            .commit_ready_for_serving(repo_dir, &ref_manifest.commit)
            .await
        {
            return Err(GitCacheError::NotFound(format!(
                "ref manifest `{}` restored generation `{}` but commit `{}` is incomplete",
                ref_manifest.ref_name, ref_manifest.generation, ref_manifest.commit
            )));
        }

        if existing_commit_manifest.is_none() {
            self.manifests().write_commit(&commit_manifest).await?;
        }

        for (ref_name, commit) in self.state.git.for_each_ref(repo_dir, "refs/heads").await? {
            match public_refs_before.get(&ref_name) {
                Some(previous) if previous != &commit => {
                    self.state
                        .git
                        .update_ref(repo_dir, &ref_name, previous.as_str())
                        .await?;
                }
                Some(_) => {}
                None => {
                    self.state.git.delete_ref(repo_dir, &ref_name).await?;
                }
            }
        }

        self.apply_restored_ref_manifest_refs(
            repo_dir,
            branch,
            ref_manifest,
            publish_public_ref,
            set_public_head,
        )
        .await?;

        Ok(())
    }

    #[cfg(test)]
    async fn apply_restored_ref_manifest_refs(
        &self,
        repo_dir: &FsPath,
        branch: &BranchName,
        ref_manifest: &RefManifest,
        publish_public_ref: bool,
        set_public_head: bool,
    ) -> CoreResult<()> {
        self.state
            .git
            .update_ref(
                repo_dir,
                &format!("refs/cache/upstream/heads/{branch}"),
                ref_manifest.commit.as_str(),
            )
            .await?;

        if publish_public_ref {
            self.state
                .git
                .update_ref(
                    repo_dir,
                    &ref_manifest.ref_name,
                    ref_manifest.commit.as_str(),
                )
                .await?;

            if set_public_head {
                self.state
                    .git
                    .symbolic_ref(repo_dir, "HEAD", &ref_manifest.ref_name)
                    .await?;
            }
        }

        Ok(())
    }

    pub(super) async fn resolve_short_commit(
        &self,
        repo_dir: &FsPath,
        short_commit: &ShortCommitSha,
    ) -> CoreResult<CommitSha> {
        let rev = format!("{}^{{commit}}", short_commit.as_str());
        let output = self
            .state
            .git
            .rev_parse(repo_dir, &rev)
            .await
            .map_err(|err| {
                GitCacheError::NotFound(format!(
                    "short commit `{short_commit}` could not be resolved unambiguously: {err}"
                ))
            })?;
        CommitSha::parse(output)
    }

    pub(super) async fn resolve_short_commit_from_upstream_refs(
        &self,
        repo_dir: &FsPath,
        short_commit: &ShortCommitSha,
    ) -> CoreResult<CommitSha> {
        let commit = self.resolve_short_commit(repo_dir, short_commit).await?;
        if self
            .commit_reachable_from_upstream_refs(repo_dir, &commit)
            .await?
        {
            return Ok(commit);
        }

        Err(GitCacheError::NotFound(format!(
            "short commit `{short_commit}` was not found in freshly fetched upstream refs"
        )))
    }

    pub(super) async fn commit_reachable_from_upstream_refs(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> CoreResult<bool> {
        let contains = format!("--contains={}", commit.as_str());
        let output = self
            .state
            .git
            .run(
                Some(repo_dir),
                [
                    "for-each-ref",
                    "--format=%(refname)",
                    contains.as_str(),
                    "refs/cache/upstream/heads",
                ],
            )
            .await?;
        let text = output.stdout_utf8("for-each-ref")?;
        Ok(text.lines().any(|line| !line.trim().is_empty()))
    }

    pub async fn fetch_all_refs(&self, repo: &RepoKey, repo_dir: &FsPath) -> CoreResult<()> {
        let _mutation_lock = self.lock_repo_mutation(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let remote = self.upstream_url(repo)?;
        self.upstream_git(&remote)?
            .fetch_all_heads(repo_dir, &remote, git_cache_git::FetchOptions::default())
            .await?;
        Ok(())
    }

    pub(super) async fn ls_remote_branch(
        &self,
        repo: &RepoKey,
        branch: &BranchName,
    ) -> CoreResult<CommitSha> {
        let remote = self.upstream_url(repo)?;
        let refs = self.upstream_git(&remote)?.ls_remote_heads(&remote).await?;
        let sha = refs.refs.get(branch.as_str()).ok_or_else(|| {
            GitCacheError::NotFound(format!("branch `{branch}` was verified absent upstream"))
        })?;
        CommitSha::parse(sha)
    }

    pub(super) async fn resolve_default_branch(&self, repo: &RepoKey) -> CoreResult<BranchName> {
        let remote = self.upstream_url(repo)?;
        self.upstream_git(&remote)?
            .ls_remote_default_branch(&remote)
            .await
            .and_then(BranchName::parse)
    }

    pub fn repo_dir(&self, repo: &RepoKey) -> PathBuf {
        self.state
            .config
            .cache_root
            .join("repos")
            .join(repo.local_bare_path())
    }

    pub(super) fn repo_disk_path(&self, repo: &RepoKey) -> PathBuf {
        PathBuf::from(repo.local_bare_path())
    }

    pub(super) async fn record_repo_access(&self, repo: &RepoKey) -> CoreResult<()> {
        self.state
            .disk
            .record_repo_access(self.repo_disk_path(repo))
            .await?;
        Ok(())
    }

    pub(super) async fn touch_repo_access(&self, repo: &RepoKey) -> CoreResult<()> {
        self.state.disk.note_repo_access(self.repo_disk_path(repo))
    }

    pub(super) async fn lock_repo(&self, repo: &RepoKey) -> CoreResult<RepoLock> {
        self.state.disk.lock_repo(self.repo_disk_path(repo)).await
    }

    pub(super) async fn lock_repo_mutation(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<OwnedMutexGuard<()>> {
        let repo_path = self.repo_disk_path(repo);
        let lock = {
            let mut locks = self.state.repo_mutation_locks.lock().await;
            Arc::clone(
                locks
                    .entry(repo_path)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        Ok(lock.lock_owned().await)
    }

    pub(super) async fn reset_invalid_repo_cache(&self, repo: &RepoKey) -> CoreResult<()> {
        self.state
            .disk
            .invalidate_repo(self.repo_disk_path(repo))
            .await
    }

    pub fn validate_host(&self, repo: &RepoKey) -> CoreResult<()> {
        if self
            .state
            .config
            .allowed_upstream_hosts
            .iter()
            .any(|host| host == repo.host())
        {
            Ok(())
        } else {
            Err(GitCacheError::Validation(format!(
                "upstream host `{}` is not allowlisted",
                repo.host()
            )))
        }
    }

    pub fn upstream_url(&self, repo: &RepoKey) -> CoreResult<String> {
        if let Some(root) = &self.state.config.upstream_root {
            return Ok(root.join(repo.local_bare_path()).display().to_string());
        }

        Ok(format!(
            "https://{}/{}/{}.git",
            repo.host(),
            repo.owner(),
            repo.name()
        ))
    }
}

/// Read a bare repo's `shallow` file into the set of shallow graft-boundary
/// commit ids. An absent file (a complete repo) yields an empty set.
async fn read_shallow_commits(repo_dir: &FsPath) -> CoreResult<HashSet<String>> {
    match fs::read_to_string(repo_dir.join("shallow")).await {
        Ok(contents) => Ok(contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(err) => Err(err.into()),
    }
}

pub fn repo_from_git_path(repo_path: &str) -> CoreResult<RepoKey> {
    let Some((repo, suffix)) = repo_path.split_once(".git") else {
        return Err(GitCacheError::Validation(format!(
            "git repo path `{repo_path}` must end in .git"
        )));
    };
    if !suffix.is_empty() && !suffix.starts_with('/') {
        return Err(GitCacheError::Validation(format!(
            "git repo path `{repo_path}` has an invalid .git suffix"
        )));
    }
    RepoKey::parse(repo)
}
