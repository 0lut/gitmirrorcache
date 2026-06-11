use super::util::hex_lower;
use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const MAX_GENERATION_MANIFEST_SCAN_KEYS: usize = 10_000;
const HYDRATE_PACK_DOWNLOAD_CONCURRENCY: usize = 4;
const HEAD_CAS_MAX_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionReport {
    pub repo: RepoKey,
    pub old_pack_count: usize,
    pub old_generations: Vec<GenerationId>,
    pub new_generation: GenerationId,
    pub bytes_reclaimed: u64,
}

impl Materializer {
    pub(super) async fn hydrate_commit(&self, manifest: &CommitManifest) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(&manifest.repo).await?;
        self.hydrate_commit_in_repo(&repo_dir, manifest).await
    }

    pub(super) async fn hydrate_commit_in_repo(
        &self,
        repo_dir: &FsPath,
        manifest: &CommitManifest,
    ) -> CoreResult<()> {
        let started = Instant::now();
        if self
            .commit_ready_for_serving(repo_dir, &manifest.commit)
            .await
        {
            info!(
                repo = %manifest.repo,
                commit = %manifest.commit,
                generation = %manifest.generation,
                elapsed_ms = elapsed_ms(started),
                "hydrate commit skipped: commit already ready"
            );
            return Ok(());
        }

        self.hydrate_generation(&manifest.repo, repo_dir, manifest.generation)
            .await?;
        if !self
            .commit_ready_for_serving(repo_dir, &manifest.commit)
            .await
        {
            return Err(GitCacheError::NotFound(format!(
                "hydrated generation `{}` did not contain complete commit `{}`",
                manifest.generation, manifest.commit
            )));
        }
        info!(
            repo = %manifest.repo,
            commit = %manifest.commit,
            generation = %manifest.generation,
            elapsed_ms = elapsed_ms(started),
            "hydrated commit from generation"
        );
        Ok(())
    }

    /// Hydrate a generation snapshot into the local repo: download each
    /// content-addressed pack the manifest references (in parallel, skipping
    /// packs already indexed locally), index them, then apply the manifest's
    /// full ref snapshot.
    pub(super) async fn hydrate_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let started = Instant::now();
        let manifest = self
            .get_generation_manifest(repo, generation)
            .await?
            .ok_or_else(|| {
                GitCacheError::NotFound(format!("generation manifest `{generation}` not found"))
            })?;
        info!(
            %repo,
            %generation,
            pack_count = manifest.packs.len(),
            ref_count = manifest.refs.len(),
            "hydrate generation started"
        );

        let pack_dir = repo_dir.join("objects").join("pack");
        fs::create_dir_all(&pack_dir).await?;
        let mut missing: Vec<PackInfo> = Vec::new();
        for pack in &manifest.packs {
            let idx_path = pack_dir.join(format!("pack-{}.idx", pack.sha256));
            if !idx_path.exists() {
                missing.push(pack.clone());
            }
        }

        if !missing.is_empty() {
            let total_len: u64 = missing.iter().map(|pack| pack.len).sum();
            let reservation = self.state.disk.reserve(total_len).await?;
            let temp_path = reservation.temp_path()?;
            fs::create_dir_all(&temp_path).await?;

            let download_started = Instant::now();
            let semaphore = Arc::new(Semaphore::new(HYDRATE_PACK_DOWNLOAD_CONCURRENCY));
            let mut join_set: JoinSet<CoreResult<(PackInfo, PathBuf)>> = JoinSet::new();
            for pack in missing.iter().cloned() {
                let store = Arc::clone(&self.state.store);
                let semaphore = Arc::clone(&semaphore);
                let download_path = temp_path.join(format!("pack-{}.pack", pack.sha256));
                join_set.spawn(async move {
                    let _permit = semaphore.acquire_owned().await.map_err(|_| {
                        GitCacheError::Internal("pack download semaphore closed".into())
                    })?;
                    if !store.get_file(&pack.key, &download_path).await? {
                        return Err(GitCacheError::NotFound(format!(
                            "pack `{}` not found",
                            pack.key
                        )));
                    }
                    let (len, sha256) = file_len_and_sha256(&download_path).await?;
                    if len != pack.len {
                        return Err(GitCacheError::Validation(format!(
                            "pack `{}` length mismatch: expected {}, got {len}",
                            pack.key, pack.len
                        )));
                    }
                    if !sha256.eq_ignore_ascii_case(&pack.sha256) {
                        return Err(GitCacheError::Validation(format!(
                            "pack `{}` sha256 mismatch",
                            pack.key
                        )));
                    }
                    Ok((pack, download_path))
                });
            }

            let mut downloaded = Vec::with_capacity(missing.len());
            let mut first_error = None;
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(entry)) => downloaded.push(entry),
                    Ok(Err(err)) => {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                        join_set.abort_all();
                    }
                    Err(err) => {
                        if first_error.is_none() {
                            first_error = Some(GitCacheError::Internal(format!(
                                "pack download task failed: {err}"
                            )));
                        }
                        join_set.abort_all();
                    }
                }
            }
            if let Some(err) = first_error {
                reservation.release().await?;
                return Err(err);
            }
            info!(
                %repo,
                %generation,
                pack_count = downloaded.len(),
                total_len,
                elapsed_ms = elapsed_ms(download_started),
                "downloaded generation packs"
            );

            let index_started = Instant::now();
            for (pack, download_path) in downloaded {
                let final_path = pack_dir.join(format!("pack-{}.pack", pack.sha256));
                if fs::rename(&download_path, &final_path).await.is_err() {
                    fs::copy(&download_path, &final_path).await?;
                    let _ = fs::remove_file(&download_path).await;
                }
                self.state.git.index_pack(repo_dir, &final_path).await?;
            }
            info!(
                %repo,
                %generation,
                elapsed_ms = elapsed_ms(index_started),
                "indexed generation packs"
            );
            reservation.release().await?;
        }

        let refs_started = Instant::now();
        let updates: Vec<(String, CommitSha)> = manifest
            .refs
            .iter()
            .map(|(ref_name, commit)| (ref_name.clone(), commit.clone()))
            .collect();
        if !updates.is_empty() {
            self.state.git.update_refs_batch(repo_dir, &updates).await?;
        }
        if let Some(head_ref) = &manifest.head_ref {
            self.state
                .git
                .symbolic_ref(repo_dir, "HEAD", head_ref)
                .await?;
        }
        info!(
            %repo,
            %generation,
            ref_count = updates.len(),
            elapsed_ms = elapsed_ms(refs_started),
            "applied generation ref snapshot"
        );

        info!(
            %repo,
            %generation,
            pack_count = manifest.packs.len(),
            elapsed_ms = elapsed_ms(started),
            "hydrate generation finished"
        );
        Ok(())
    }

    pub async fn publish_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        branch: Option<BranchName>,
        default_branch: bool,
    ) -> CoreResult<GenerationId> {
        self.publish_generation_with_parent_mode(
            repo,
            repo_dir,
            commit,
            branch,
            default_branch,
            true,
        )
        .await
    }

    pub(super) async fn publish_generation_without_parent(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        branch: Option<BranchName>,
        default_branch: bool,
    ) -> CoreResult<GenerationId> {
        self.publish_generation_with_parent_mode(
            repo,
            repo_dir,
            commit,
            branch,
            default_branch,
            false,
        )
        .await
    }

    async fn publish_generation_with_parent_mode(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        branch: Option<BranchName>,
        default_branch: bool,
        use_previous_head: bool,
    ) -> CoreResult<GenerationId> {
        let started = Instant::now();
        info!(
            %repo,
            %commit,
            branch = branch
                .as_ref()
                .map(|branch| branch.as_str())
                .unwrap_or("<none>"),
            default_branch,
            "publish generation started"
        );
        if let Some(existing) = self.get_commit_manifest(repo, commit).await? {
            if existing.complete {
                if let Some(branch) = branch {
                    let ref_manifest = RefManifest {
                        repo: repo.clone(),
                        ref_name: branch.ref_name(),
                        commit: commit.clone(),
                        generation: existing.generation,
                        verified_at: Utc::now(),
                    };
                    self.manifests().write_ref(&ref_manifest).await?;
                }
                if default_branch {
                    self.put_default_manifest(repo, commit).await?;
                }

                info!(
                    %repo,
                    %commit,
                    generation = %existing.generation,
                    elapsed_ms = elapsed_ms(started),
                    "publish generation skipped: commit manifest already complete"
                );
                return Ok(existing.generation);
            }
        }

        let head_started = Instant::now();
        let previous_head = if use_previous_head {
            let previous_head = self.manifests().repo_head(repo).await?;
            info!(
                %repo,
                previous_generation = previous_head.as_ref().map(|head| head.generation.to_string()).unwrap_or_else(|| "<none>".into()),
                previous_tip_count = previous_head.as_ref().map(|head| head.tip_commits.len()).unwrap_or(0),
                elapsed_ms = elapsed_ms(head_started),
                "loaded repo generation head"
            );
            previous_head
        } else {
            info!(
                %repo,
                elapsed_ms = elapsed_ms(head_started),
                "skipped repo generation head for standalone generation publish"
            );
            None
        };
        let previous_manifest = match previous_head.as_ref() {
            Some(head) => self.get_generation_manifest(repo, head.generation).await?,
            None => None,
        };
        let previous_tips = previous_head
            .as_ref()
            .map(|head| head.tip_commits.as_slice())
            .unwrap_or(&[]);
        let generation = GenerationId::new();

        let reservation = self.state.disk.reserve(1024 * 1024 * 64).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let pack_prefix = temp_path.join("generation");
        let now = Utc::now();
        let mut tip_commits = previous_head
            .as_ref()
            .map(|head| head.tip_commits.clone())
            .unwrap_or_default();

        let include = self
            .state
            .git
            .for_each_ref_commits(repo_dir, "refs")
            .await?;
        let pack_started = Instant::now();
        let mut incremental = previous_manifest.is_some() && !previous_tips.is_empty();
        let pack_path = if incremental {
            match self
                .state
                .git
                .pack_objects_revs(repo_dir, &pack_prefix, &include, previous_tips)
                .await
            {
                Ok(path) => {
                    info!(
                        %repo,
                        %generation,
                        previous_tip_count = previous_tips.len(),
                        elapsed_ms = elapsed_ms(pack_started),
                        "created incremental generation pack"
                    );
                    path
                }
                Err(err) => {
                    warn!(
                        %repo,
                        %err,
                        elapsed_ms = elapsed_ms(pack_started),
                        "incremental pack failed, falling back to full pack"
                    );
                    incremental = false;
                    tip_commits.clear();
                    let fallback_started = Instant::now();
                    let path = self
                        .state
                        .git
                        .pack_objects_revs(repo_dir, &pack_prefix, &include, &[])
                        .await?;
                    info!(
                        %repo,
                        %generation,
                        elapsed_ms = elapsed_ms(fallback_started),
                        "created fallback full generation pack"
                    );
                    path
                }
            }
        } else {
            incremental = false;
            tip_commits.clear();
            let path = self
                .state
                .git
                .pack_objects_revs(repo_dir, &pack_prefix, &include, &[])
                .await?;
            info!(
                %repo,
                %generation,
                elapsed_ms = elapsed_ms(pack_started),
                "created full generation pack"
            );
            path
        };
        push_unique_commit(&mut tip_commits, commit.clone());

        let (pack_len, pack_sha256) = file_len_and_sha256(&pack_path).await?;
        let new_pack = PackInfo {
            key: pack_key(repo, &pack_sha256)?,
            len: pack_len,
            sha256: pack_sha256,
            kind: if incremental {
                PackKind::Delta
            } else {
                PackKind::Base
            },
        };
        let mut packs = if incremental {
            previous_manifest
                .as_ref()
                .map(|manifest| manifest.packs.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !packs.iter().any(|pack| pack.key == new_pack.key) {
            packs.push(new_pack.clone());
        }

        let refs: BTreeMap<String, CommitSha> = self
            .state
            .git
            .for_each_ref(repo_dir, "refs")
            .await?
            .into_iter()
            .collect();
        let head_ref = self
            .state
            .git
            .symbolic_ref_read(repo_dir, "HEAD")
            .await
            .ok()
            .filter(|target| refs.contains_key(target));

        let mut manifest_commits = vec![commit.clone()];
        let manifest_scan_started = Instant::now();
        for ref_prefix in ["refs/cache/upstream/heads", "refs/cache/commits"] {
            for candidate in self
                .state
                .git
                .for_each_ref_commits(repo_dir, ref_prefix)
                .await?
            {
                if self.get_commit_manifest(repo, &candidate).await?.is_none() {
                    push_unique_commit(&mut manifest_commits, candidate);
                }
            }
        }
        info!(
            %repo,
            %generation,
            manifest_commit_count = manifest_commits.len(),
            elapsed_ms = elapsed_ms(manifest_scan_started),
            "collected generation manifest commits"
        );

        let generation_manifest = GenerationManifest {
            repo: repo.clone(),
            generation,
            created_at: now,
            verified_at: Some(now),
            packs,
            refs,
            head_ref,
            commits: manifest_commits.clone(),
        };
        let mut manifests = PublishManifests {
            commits: manifest_commits
                .into_iter()
                .map(|commit| CommitManifest {
                    repo: repo.clone(),
                    commit,
                    generation,
                    complete: true,
                    verified_at: now,
                })
                .collect(),
            refs: Vec::new(),
        };

        if let Some(branch) = branch {
            let ref_manifest = RefManifest {
                repo: repo.clone(),
                ref_name: branch.ref_name(),
                commit: commit.clone(),
                generation,
                verified_at: now,
            };
            manifests.refs.push(ref_manifest);
        }
        let default_ref = if default_branch {
            Some(RefManifest {
                repo: repo.clone(),
                ref_name: "HEAD".into(),
                commit: commit.clone(),
                generation,
                verified_at: now,
            })
        } else {
            None
        };
        let head = RepoGenerationHead {
            repo: repo.clone(),
            generation,
            tip_commits,
            updated_at: now,
        };

        let publish_started = Instant::now();
        let pack_count = generation_manifest.packs.len();
        GenerationPublish::with_manifests(generation_manifest, manifests)
            .publish_pack_files(
                &*self.state.store,
                &[(new_pack.key.clone(), pack_path.clone())],
            )
            .await?;
        self.finish_generation_publish(repo, &head, default_ref)
            .await?;
        info!(
            %repo,
            %generation,
            pack_len,
            elapsed_ms = elapsed_ms(publish_started),
            "published generation pack"
        );

        reservation.release().await?;

        if pack_count > self.state.config.compaction.chain_depth_threshold as usize {
            self.enqueue_inline_compaction(repo.clone(), generation);
        }

        info!(
            %repo,
            %commit,
            %generation,
            elapsed_ms = elapsed_ms(started),
            "publish generation finished"
        );
        Ok(generation)
    }

    fn enqueue_inline_compaction(&self, repo: RepoKey, generation: GenerationId) {
        if !self.state.config.compaction.inline {
            return;
        }

        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            let materializer = Materializer::new(state);
            if let Err(err) = Box::pin(materializer.compact_generation_chain(&repo)).await {
                warn!(%repo, %generation, %err, "inline generation compaction failed");
            }
        });
    }

    async fn finish_generation_publish(
        &self,
        repo: &RepoKey,
        head: &RepoGenerationHead,
        default_ref: Option<RefManifest>,
    ) -> CoreResult<()> {
        if let Some(default_ref) = default_ref {
            self.manifests()
                .write_default_ref(repo, &default_ref)
                .await?;
        }

        for _ in 0..HEAD_CAS_MAX_ATTEMPTS {
            let current = self.manifests().repo_head_versioned(repo).await?;
            let (current_head, version) = match &current {
                Some((head, version)) => (Some(head), Some(version)),
                None => (None, None),
            };
            if current_head
                .map(|current| current.updated_at > head.updated_at)
                .unwrap_or(false)
            {
                return Ok(());
            }
            if self
                .manifests()
                .write_repo_head_if_version_matches(head, version)
                .await?
            {
                return Ok(());
            }
        }
        warn!(
            %repo,
            generation = %head.generation,
            "generation head moved repeatedly during publish; leaving newer head in place"
        );
        Ok(())
    }

    pub async fn compact_generation_chain(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<Option<CompactionReport>> {
        let threshold = self.state.config.compaction.chain_depth_threshold as usize;
        Box::pin(self.compact_generation_chain_inner(repo, threshold, false)).await
    }

    pub async fn compact_generation_chain_dry_run(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<Option<CompactionReport>> {
        let threshold = self.state.config.compaction.chain_depth_threshold as usize;
        Box::pin(self.compact_generation_chain_inner(repo, threshold, true)).await
    }

    /// Compact a repo whose head generation references too many packs:
    /// hydrate the head snapshot, repack the local repo into a single pack,
    /// publish it as a fresh self-contained generation, then delete the old
    /// generation manifests and any packs no longer referenced.
    pub(super) async fn compact_generation_chain_inner(
        &self,
        repo: &RepoKey,
        threshold: usize,
        dry_run: bool,
    ) -> CoreResult<Option<CompactionReport>> {
        let outer_repo_lock = if dry_run {
            None
        } else {
            Some(self.lock_repo(repo).await?)
        };
        let Some((head, head_version)) = self.manifests().repo_head_versioned(repo).await? else {
            return Ok(None);
        };
        let Some(head_manifest) = self.get_generation_manifest(repo, head.generation).await? else {
            return Ok(None);
        };
        if head_manifest.packs.len() <= threshold {
            return Ok(None);
        }

        let old_generations = self.list_generation_ids(repo).await?;
        let mut old_packs: BTreeMap<String, u64> = BTreeMap::new();
        let mut old_commits = Vec::new();
        for generation in &old_generations {
            let Some(manifest) = self.get_generation_manifest(repo, *generation).await? else {
                continue;
            };
            for pack in &manifest.packs {
                old_packs.insert(pack.key.clone(), pack.len);
            }
            for commit in &manifest.commits {
                push_unique_commit(&mut old_commits, commit.clone());
            }
        }
        let old_generation_set: HashSet<GenerationId> = old_generations.iter().copied().collect();
        let new_generation = GenerationId::new();
        if dry_run {
            let bytes_reclaimed = old_packs.values().sum();
            return Ok(Some(CompactionReport {
                repo: repo.clone(),
                old_pack_count: head_manifest.packs.len(),
                old_generations,
                new_generation,
                bytes_reclaimed,
            }));
        }

        let _repo_lock = outer_repo_lock;
        let repo_dir = self.ensure_repo_dir(repo).await?;
        Box::pin(self.hydrate_generation(repo, &repo_dir, head.generation)).await?;
        self.state.git.repack_for_serving(&repo_dir).await?;

        let pack_dir = repo_dir.join("objects").join("pack");
        let mut local_packs: Vec<(String, PathBuf)> = Vec::new();
        let mut packs: Vec<PackInfo> = Vec::new();
        let mut entries = fs::read_dir(&pack_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("pack") {
                continue;
            }
            let (len, sha256) = file_len_and_sha256(&path).await?;
            let key = pack_key(repo, &sha256)?;
            if packs.iter().any(|pack| pack.key == key) {
                continue;
            }
            packs.push(PackInfo {
                key: key.clone(),
                len,
                sha256,
                kind: PackKind::Base,
            });
            local_packs.push((key, path));
        }
        if packs.is_empty() {
            return Err(GitCacheError::Internal(format!(
                "compaction repack for `{repo}` produced no pack files"
            )));
        }

        let refs: BTreeMap<String, CommitSha> = self
            .state
            .git
            .for_each_ref(&repo_dir, "refs")
            .await?
            .into_iter()
            .collect();
        let head_ref = self
            .state
            .git
            .symbolic_ref_read(&repo_dir, "HEAD")
            .await
            .ok()
            .filter(|target| refs.contains_key(target));

        let now = Utc::now();
        let generation_manifest = GenerationManifest {
            repo: repo.clone(),
            generation: new_generation,
            created_at: now,
            verified_at: Some(now),
            packs: packs.clone(),
            refs,
            head_ref,
            commits: old_commits.clone(),
        };
        let new_head = RepoGenerationHead {
            repo: repo.clone(),
            generation: new_generation,
            tip_commits: head.tip_commits.clone(),
            updated_at: now,
        };
        GenerationPublish::new(generation_manifest)
            .publish_pack_files(&*self.state.store, &local_packs)
            .await?;

        self.repoint_manifests_after_compaction(repo, &old_generation_set, new_generation)
            .await?;
        for commit in old_commits {
            let manifest = CommitManifest {
                repo: repo.clone(),
                commit,
                generation: new_generation,
                complete: true,
                verified_at: now,
            };
            self.manifests().write_commit(&manifest).await?;
        }
        if !self
            .manifests()
            .write_repo_head_if_version_matches(&new_head, Some(&head_version))
            .await?
        {
            warn!(
                %repo,
                %new_generation,
                "generation head changed during compaction; skipping cleanup of old packs"
            );
            return Ok(None);
        }

        let retained_keys: HashSet<&str> = packs.iter().map(|pack| pack.key.as_str()).collect();
        let mut bytes_reclaimed = 0_u64;
        for (key, len) in &old_packs {
            if retained_keys.contains(key.as_str()) {
                continue;
            }
            self.state.store.delete(key).await?;
            bytes_reclaimed = bytes_reclaimed.saturating_add(*len);
        }
        for generation in &old_generations {
            self.state
                .store
                .delete(&generation_manifest_key(repo, *generation))
                .await?;
        }

        Ok(Some(CompactionReport {
            repo: repo.clone(),
            old_pack_count: head_manifest.packs.len(),
            old_generations,
            new_generation,
            bytes_reclaimed,
        }))
    }

    /// List the generation ids that currently have a manifest in the store.
    pub(super) async fn list_generation_ids(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<Vec<GenerationId>> {
        let prefix = generation_manifest_prefix(repo);
        let keys = self
            .state
            .store
            .list_prefix(&prefix, Some(MAX_GENERATION_MANIFEST_SCAN_KEYS))
            .await?;
        if keys.len() >= MAX_GENERATION_MANIFEST_SCAN_KEYS {
            return Err(GitCacheError::Conflict(format!(
                "too many generation manifests for `{repo}` to compact safely"
            )));
        }
        let mut generations = Vec::new();
        for key in keys {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(generation) = rest.strip_suffix("/manifest.json") else {
                continue;
            };
            let Ok(uuid) = uuid::Uuid::parse_str(generation) else {
                warn!(key, "skipping malformed generation manifest key");
                continue;
            };
            generations.push(GenerationId(uuid));
        }
        Ok(generations)
    }

    pub(super) async fn repoint_manifests_after_compaction(
        &self,
        repo: &RepoKey,
        old_generations: &HashSet<GenerationId>,
        new_generation: GenerationId,
    ) -> CoreResult<()> {
        let prefix = format!("repos/{repo}/manifests/");
        let keys = self.state.store.list_prefix(&prefix, None).await?;
        for key in keys {
            if key.contains("/manifests/commits/") && key.ends_with(".json") {
                if let Some(mut manifest) = self.manifests().commit_by_key(&key).await? {
                    if manifest.repo == *repo && old_generations.contains(&manifest.generation) {
                        manifest.generation = new_generation;
                        self.manifests().write_commit(&manifest).await?;
                    }
                }
            } else if key.contains("/manifests/refs/") && key.ends_with(".json") {
                if key == default_manifest_key(repo) {
                    continue;
                }
                if let Some(mut manifest) = self.manifests().ref_by_key(&key).await? {
                    if manifest.repo == *repo && old_generations.contains(&manifest.generation) {
                        manifest.generation = new_generation;
                        self.manifests().write_ref(&manifest).await?;
                    }
                }
            }
        }

        let default_key = default_manifest_key(repo);
        if let Some(mut manifest) = self.manifests().ref_by_key(&default_key).await? {
            if manifest.repo == *repo && old_generations.contains(&manifest.generation) {
                manifest.generation = new_generation;
                self.manifests().write_default_ref(repo, &manifest).await?;
            }
        }

        Ok(())
    }

    pub(super) async fn put_default_manifest(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CoreResult<()> {
        let commit_manifest = self
            .get_commit_manifest(repo, commit)
            .await?
            .ok_or_else(|| {
                GitCacheError::NotFound(format!("commit manifest `{commit}` missing"))
            })?;
        let manifest = RefManifest {
            repo: repo.clone(),
            ref_name: "HEAD".into(),
            commit: commit.clone(),
            generation: commit_manifest.generation,
            verified_at: Utc::now(),
        };
        self.manifests().write_default_ref(repo, &manifest).await
    }

    pub async fn get_commit_manifest(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CoreResult<Option<CommitManifest>> {
        self.manifests().commit(repo, commit).await
    }

    pub(super) async fn get_generation_manifest(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Option<GenerationManifest>> {
        self.manifests().generation(repo, generation).await
    }
}

pub fn default_manifest_key(repo: &RepoKey) -> String {
    format!("repos/{}/manifests/refs/default.json", repo.as_str())
}

pub(super) async fn file_len_and_sha256(path: &FsPath) -> CoreResult<(u64, String)> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        len += read as u64;
        hasher.update(&buffer[..read]);
    }

    Ok((len, hex_lower(&hasher.finalize())))
}

pub(super) fn push_unique_commit(commits: &mut Vec<CommitSha>, commit: CommitSha) {
    if !commits.iter().any(|existing| existing == &commit) {
        commits.push(commit);
    }
}
