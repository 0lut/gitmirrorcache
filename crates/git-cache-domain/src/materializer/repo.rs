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
