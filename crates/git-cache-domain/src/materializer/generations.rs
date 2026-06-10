use super::util::hex_lower;
use super::*;
use sha2::{Digest, Sha256};

const VERIFIED_GENERATION_SCHEMA_VERSION: u32 = 2;
const VERIFIED_GENERATION_VERIFIER_VERSION: u32 = 1;
const VERIFIED_GENERATION_FSCK_MODE: &str = "connectivity-only";
const PENDING_GENERATION_PREFIX: &str = "pending-generations/";
const MAX_PENDING_GENERATION_SCAN_KEYS: usize = 10_000;
const GENERATION_VERIFICATION_MAX_ATTEMPTS: usize = 3;
const GENERATION_VERIFICATION_RETRY_DELAY: StdDuration = StdDuration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionReport {
    pub repo: RepoKey,
    pub old_chain_depth: usize,
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

    pub(super) async fn hydrate_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let started = Instant::now();
        let chain = self.generation_chain(repo, generation).await?;
        info!(
            %repo,
            %generation,
            chain_len = chain.len(),
            "hydrate generation started"
        );

        for generation_manifest in chain.iter().rev() {
            let bundle_started = Instant::now();
            let manifest_started = Instant::now();
            let verification = self
                .manifests()
                .verified_generation(repo, generation_manifest.generation)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!(
                        "verified generation manifest `{}` not found",
                        generation_manifest.generation
                    ))
                })?;
            info!(
                %repo,
                generation = %generation_manifest.generation,
                elapsed_ms = elapsed_ms(manifest_started),
                "loaded verified generation manifest"
            );
            let head_started = Instant::now();
            let bundle_meta = self
                .state
                .store
                .head(&generation_manifest.bundle_key)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!(
                        "bundle `{}` not found",
                        generation_manifest.bundle_key
                    ))
                })?;
            info!(
                %repo,
                generation = %generation_manifest.generation,
                bundle_key = %generation_manifest.bundle_key,
                bundle_len = bundle_meta.len,
                elapsed_ms = elapsed_ms(head_started),
                "loaded generation bundle metadata"
            );

            validate_verified_generation(generation_manifest, &verification, bundle_meta.len)?;

            let reserve_started = Instant::now();
            let reservation = self.state.disk.reserve(verification.bundle_len).await?;
            let temp_path = reservation.temp_path()?;
            fs::create_dir_all(&temp_path).await?;
            let bundle_path = temp_path.join("hydrate.bundle");
            info!(
                %repo,
                generation = %generation_manifest.generation,
                bundle_len = verification.bundle_len,
                elapsed_ms = elapsed_ms(reserve_started),
                "reserved disk for generation hydrate"
            );
            let download_started = Instant::now();
            if !self
                .state
                .store
                .get_file(&generation_manifest.bundle_key, &bundle_path)
                .await?
            {
                return Err(GitCacheError::NotFound(format!(
                    "bundle `{}` not found",
                    generation_manifest.bundle_key
                )));
            }
            info!(
                %repo,
                generation = %generation_manifest.generation,
                bundle_key = %generation_manifest.bundle_key,
                bundle_len = verification.bundle_len,
                elapsed_ms = elapsed_ms(download_started),
                "downloaded generation bundle"
            );
            let checksum_started = Instant::now();
            let (bundle_len, bundle_sha256) = file_len_and_sha256(&bundle_path).await?;
            if bundle_len != verification.bundle_len {
                return Err(GitCacheError::Validation(format!(
                    "bundle `{}` length mismatch: expected {}, got {}",
                    generation_manifest.bundle_key, verification.bundle_len, bundle_len
                )));
            }
            if !bundle_sha256.eq_ignore_ascii_case(&verification.bundle_sha256) {
                return Err(GitCacheError::Validation(format!(
                    "bundle `{}` sha256 mismatch",
                    generation_manifest.bundle_key
                )));
            }
            info!(
                %repo,
                generation = %generation_manifest.generation,
                bundle_len,
                elapsed_ms = elapsed_ms(checksum_started),
                "verified generation bundle checksum"
            );
            let fetch_started = Instant::now();
            self.state.git.fetch_bundle(repo_dir, &bundle_path).await?;
            info!(
                %repo,
                generation = %generation_manifest.generation,
                elapsed_ms = elapsed_ms(fetch_started),
                "fetched generation bundle into local repo"
            );
            reservation.release().await?;
            info!(
                %repo,
                generation = %generation_manifest.generation,
                elapsed_ms = elapsed_ms(bundle_started),
                "hydrated generation bundle"
            );
        }
        info!(
            %repo,
            %generation,
            chain_len = chain.len(),
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
        let previous_generation = previous_head.as_ref().map(|head| head.generation);
        let previous_tips = previous_head
            .as_ref()
            .map(|head| head.tip_commits.as_slice())
            .unwrap_or(&[]);
        let generation = GenerationId::new();
        let bundle_key = bundle_key(repo, generation);

        let reservation = self.state.disk.reserve(1024 * 1024 * 64).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let bundle_path = temp_path.join("generation.bundle");
        let now = Utc::now();
        let mut parent_generation = previous_generation;
        let mut tip_commits = previous_head
            .as_ref()
            .map(|head| head.tip_commits.clone())
            .unwrap_or_default();

        let bundle_started = Instant::now();
        if previous_tips.is_empty() {
            self.state
                .git
                .bundle_create_all(repo_dir, &bundle_path)
                .await?;
            info!(
                %repo,
                %generation,
                elapsed_ms = elapsed_ms(bundle_started),
                "created full generation bundle"
            );
        } else if let Err(err) = self
            .state
            .git
            .bundle_create_incremental(repo_dir, &bundle_path, previous_tips)
            .await
        {
            warn!(
                %repo,
                %err,
                elapsed_ms = elapsed_ms(bundle_started),
                "delta bundle failed, falling back to full bundle"
            );
            let _ = fs::remove_file(&bundle_path).await;
            let fallback_started = Instant::now();
            self.state
                .git
                .bundle_create_all(repo_dir, &bundle_path)
                .await?;
            info!(
                %repo,
                %generation,
                elapsed_ms = elapsed_ms(fallback_started),
                "created fallback full generation bundle"
            );
            parent_generation = None;
            tip_commits.clear();
        } else {
            info!(
                %repo,
                %generation,
                previous_tip_count = previous_tips.len(),
                elapsed_ms = elapsed_ms(bundle_started),
                "created incremental generation bundle"
            );
        }
        push_unique_commit(&mut tip_commits, commit.clone());

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
            bundle_key,
            parent_generation,
            created_at: now,
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
        let verification = Box::pin(self.verify_local_generation_bundle(
            repo,
            repo_dir,
            generation,
            &generation_manifest,
            &bundle_path,
            &head.tip_commits,
        ))
        .await?;
        let published_verified = verification.is_some();
        let default_ref_for_verified = default_ref.clone();
        if let Some(verification) = verification {
            GenerationPublish::with_manifests(generation_manifest.clone(), manifests)
                .with_verification(verification)
                .publish_bundle_file(&*self.state.store, &bundle_path)
                .await?;
            self.finish_verified_generation_publish(repo, &head, default_ref_for_verified)
                .await?;
            info!(
                %repo,
                %generation,
                elapsed_ms = elapsed_ms(publish_started),
                "published verified generation bundle"
            );
        } else {
            GenerationPublish::with_manifests(generation_manifest, manifests)
                .publish_pending_bundle_file(&*self.state.store, &bundle_path, head, default_ref)
                .await?;
            info!(
                %repo,
                %generation,
                elapsed_ms = elapsed_ms(publish_started),
                "published pending generation bundle"
            );
        }

        reservation.release().await?;

        if published_verified {
            self.enqueue_inline_compaction(repo.clone(), generation);
        } else {
            self.enqueue_generation_verification(repo.clone(), generation);
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

    pub(super) fn enqueue_generation_verification(&self, repo: RepoKey, generation: GenerationId) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            for attempt in 1..=GENERATION_VERIFICATION_MAX_ATTEMPTS {
                let materializer = Materializer::new(Arc::clone(&state));
                let result = materializer
                    .verify_generation_with_semaphore_mode(repo.clone(), generation, true, false)
                    .await;
                match result {
                    Ok(()) => return,
                    Err(err) if attempt < GENERATION_VERIFICATION_MAX_ATTEMPTS => {
                        warn!(%repo, %generation, attempt, %err, "generation verification failed; retrying");
                        tokio::time::sleep(GENERATION_VERIFICATION_RETRY_DELAY).await;
                    }
                    Err(err) => {
                        warn!(%repo, %generation, attempt, %err, "generation verification failed");
                        return;
                    }
                }
            }
        });
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

    pub fn enqueue_pending_generation_scan(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("skipping pending generation scan: no active tokio runtime");
            return;
        };
        let materializer = self.clone();
        handle.spawn(async move {
            match materializer
                .enqueue_pending_generation_verifications()
                .await
            {
                Ok(count) => {
                    if count > 0 {
                        info!(count, "enqueued pending generation verifications");
                    }
                }
                Err(err) => warn!(%err, "pending generation scan failed"),
            }
        });
    }

    pub async fn enqueue_pending_generation_verifications(&self) -> CoreResult<usize> {
        let keys = self
            .state
            .store
            .list_prefix(
                PENDING_GENERATION_PREFIX,
                Some(MAX_PENDING_GENERATION_SCAN_KEYS),
            )
            .await?;
        let mut queued = 0usize;
        for key in keys {
            match pending_generation_from_key(&key) {
                Ok(Some((repo, generation))) => {
                    self.enqueue_generation_verification(repo, generation);
                    queued += 1;
                }
                Ok(None) => {}
                Err(err) => warn!(key, %err, "skipping malformed pending generation key"),
            }
        }
        Ok(queued)
    }

    pub(super) async fn verify_generation_with_semaphore(
        &self,
        repo: RepoKey,
        generation: GenerationId,
        inline_compaction: bool,
    ) -> CoreResult<()> {
        self.verify_generation_with_semaphore_mode(repo, generation, inline_compaction, true)
            .await
    }

    async fn verify_generation_with_semaphore_mode(
        &self,
        repo: RepoKey,
        generation: GenerationId,
        inline_compaction: bool,
        allow_full_chain: bool,
    ) -> CoreResult<()> {
        let permit = self
            .state
            .generation_verification_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                GitCacheError::Internal("generation verification semaphore closed".into())
            })?;
        Box::pin(self.verify_generation_inner(repo.clone(), generation, allow_full_chain)).await?;
        drop(permit);

        if inline_compaction && self.state.config.compaction.inline {
            if let Err(err) = Box::pin(self.compact_generation_chain(&repo)).await {
                warn!(%repo, %generation, %err, "inline generation compaction failed");
            }
        }
        Ok(())
    }

    pub(super) async fn verify_generation_inner(
        &self,
        repo: RepoKey,
        generation: GenerationId,
        allow_full_chain: bool,
    ) -> CoreResult<()> {
        let Some(pending) = self
            .manifests()
            .pending_generation(&repo, generation)
            .await?
        else {
            return Ok(());
        };

        if let Some(verification) =
            Box::pin(self.verify_generation_from_local_repo(&repo, generation, &pending)).await?
        {
            Box::pin(self.publish_verified_pending_generation(
                &repo,
                generation,
                pending,
                verification,
            ))
            .await?;
            return Ok(());
        }

        if !allow_full_chain {
            info!(
                %repo,
                %generation,
                "generation verification left pending: local fast path was unavailable"
            );
            return Ok(());
        }

        let chain = self
            .generation_chain_for_verification(&repo, generation)
            .await?;
        let mut bundle_metas = Vec::with_capacity(chain.len());
        let mut total_len = 0_u64;
        for manifest in chain.iter().rev() {
            let meta = self
                .state
                .store
                .head(&manifest.bundle_key)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!("bundle `{}` not found", manifest.bundle_key))
                })?;
            total_len = total_len.saturating_add(meta.len);
            bundle_metas.push((manifest, meta.len));
        }

        let reservation = self.state.disk.reserve(total_len).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let verify_repo = temp_path.join("verify.git");
        self.state.git.init_bare(&verify_repo).await?;

        let mut verifications = Vec::with_capacity(bundle_metas.len());
        let now = Utc::now();
        let tip_commits = pending.head.tip_commits.clone();

        for (manifest, object_len) in bundle_metas {
            let bundle_path = temp_path.join(format!("{}.bundle", manifest.generation));
            if !self
                .state
                .store
                .get_file(&manifest.bundle_key, &bundle_path)
                .await?
            {
                return Err(GitCacheError::NotFound(format!(
                    "bundle `{}` not found",
                    manifest.bundle_key
                )));
            }
            let (bundle_len, bundle_sha256) = file_len_and_sha256(&bundle_path).await?;
            if bundle_len != object_len {
                return Err(GitCacheError::Validation(format!(
                    "bundle `{}` length changed during verification: expected {}, got {}",
                    manifest.bundle_key, object_len, bundle_len
                )));
            }
            self.state
                .git
                .fetch_bundle(&verify_repo, &bundle_path)
                .await?;
            let manifest_tip_commits = if manifest.generation == generation {
                tip_commits.clone()
            } else {
                Vec::new()
            };
            verifications.push(verified_generation_manifest(
                manifest,
                bundle_len,
                bundle_sha256,
                now,
                manifest_tip_commits,
            ));
        }

        self.state.git.fsck(&verify_repo).await?;
        let mut pending_verification = None;
        for verification in verifications {
            if verification.generation == generation {
                pending_verification = Some(verification);
            } else if self
                .manifests()
                .verified_generation(&verification.repo, verification.generation)
                .await?
                .is_none()
            {
                self.manifests()
                    .write_verified_if_absent_or_matches(&verification)
                    .await?;
            }
        }
        let verification = pending_verification.ok_or_else(|| {
            GitCacheError::Internal(format!(
                "verification for generation `{generation}` was not produced"
            ))
        })?;
        Box::pin(self.publish_verified_pending_generation(
            &repo,
            generation,
            pending,
            verification,
        ))
        .await?;
        reservation.release().await?;
        info!(%repo, %generation, "generation verified");
        Ok(())
    }

    async fn verify_generation_from_local_repo(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
        pending: &PendingGenerationPublish,
    ) -> CoreResult<Option<VerifiedGenerationManifest>> {
        let repo_dir = self.repo_dir(repo);
        if !repo_dir.join("config").exists() {
            return Ok(None);
        }

        let bundle_meta = self
            .state
            .store
            .head(&pending.generation.bundle_key)
            .await?
            .ok_or_else(|| {
                GitCacheError::NotFound(format!(
                    "bundle `{}` not found",
                    pending.generation.bundle_key
                ))
            })?;
        let reservation = self.state.disk.reserve(bundle_meta.len).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let bundle_path = temp_path.join("verify-local.bundle");
        if !self
            .state
            .store
            .get_file(&pending.generation.bundle_key, &bundle_path)
            .await?
        {
            reservation.release().await?;
            return Err(GitCacheError::NotFound(format!(
                "bundle `{}` not found",
                pending.generation.bundle_key
            )));
        }

        let verification = Box::pin(self.verify_local_generation_bundle(
            repo,
            &repo_dir,
            generation,
            &pending.generation,
            &bundle_path,
            &pending.head.tip_commits,
        ))
        .await?;
        let Some(verification) = verification else {
            reservation.release().await?;
            return Ok(None);
        };

        if verification.bundle_len != bundle_meta.len {
            reservation.release().await?;
            return Err(GitCacheError::Validation(format!(
                "bundle `{}` length changed during verification: expected {}, got {}",
                pending.generation.bundle_key, bundle_meta.len, verification.bundle_len
            )));
        }
        reservation.release().await?;
        Ok(Some(verification))
    }

    async fn verify_local_generation_bundle(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        generation: GenerationId,
        generation_manifest: &GenerationManifest,
        bundle_path: &FsPath,
        tip_commits: &[CommitSha],
    ) -> CoreResult<Option<VerifiedGenerationManifest>> {
        let started = Instant::now();
        for tip in tip_commits {
            if !self.commit_ready_for_serving(repo_dir, tip).await {
                info!(
                    %repo,
                    %generation,
                    tip = %tip,
                    elapsed_ms = elapsed_ms(started),
                    "local generation verification skipped: tip not locally ready"
                );
                return Ok(None);
            }
        }

        // For HTTP-published incremental generations, the local repo already
        // has the parent history and the new objects that produced the bundle.
        // Re-fetching the whole verified parent chain into a temporary repo is
        // catastrophically expensive for repos like llvm-project and can starve
        // hot materialize/direct-git requests. When every ancestor is already
        // verified, `git bundle verify` against the local repo is enough to
        // prove the pending bundle's prerequisites without running index-pack
        // over gigabytes of parent history again.
        let mut next = generation_manifest.parent_generation;
        while let Some(parent) = next {
            if self
                .manifests()
                .verified_generation(repo, parent)
                .await?
                .is_none()
            {
                info!(
                    %repo,
                    %generation,
                    parent_generation = %parent,
                    elapsed_ms = elapsed_ms(started),
                    "local generation verification skipped: parent is not verified"
                );
                return Ok(None);
            }
            let Some(parent_manifest) = self.get_generation_manifest(repo, parent).await? else {
                info!(
                    %repo,
                    %generation,
                    parent_generation = %parent,
                    elapsed_ms = elapsed_ms(started),
                    "local generation verification skipped: parent manifest missing"
                );
                return Ok(None);
            };
            next = parent_manifest.parent_generation;
        }

        if let Err(err) = self.state.git.bundle_verify(repo_dir, bundle_path).await {
            info!(
                %repo,
                %generation,
                %err,
                elapsed_ms = elapsed_ms(started),
                "local generation verification skipped: bundle verify failed"
            );
            return Ok(None);
        }

        let (bundle_len, bundle_sha256) = file_len_and_sha256(bundle_path).await?;
        info!(
            %repo,
            %generation,
            bundle_len,
            elapsed_ms = elapsed_ms(started),
            "verified generation from local repo"
        );
        Ok(Some(verified_generation_manifest(
            generation_manifest,
            bundle_len,
            bundle_sha256,
            Utc::now(),
            tip_commits.to_vec(),
        )))
    }

    async fn finish_verified_generation_publish(
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

        let current_head = self.manifests().repo_head(repo).await?;
        if current_head
            .as_ref()
            .map(|current| current.updated_at <= head.updated_at)
            .unwrap_or(true)
        {
            self.manifests().write_repo_head(head).await?;
        }
        Ok(())
    }

    async fn publish_verified_pending_generation(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
        pending: PendingGenerationPublish,
        verification: VerifiedGenerationManifest,
    ) -> CoreResult<()> {
        GenerationPublish::with_manifests(pending.generation.clone(), pending.manifests.clone())
            .with_verification(verification)
            .publish_verified_metadata(&*self.state.store)
            .await?;

        self.finish_verified_generation_publish(repo, &pending.head, pending.default_ref)
            .await?;
        if let Err(err) = self
            .state
            .store
            .delete(&pending_generation_publish_key(repo, generation))
            .await
        {
            warn!(%repo, %generation, %err, "failed to delete pending generation publish");
        }
        Ok(())
    }

    pub(super) async fn generation_chain_for_verification(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Vec<GenerationManifest>> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(generation);
        while let Some(current) = next {
            if !seen.insert(current) {
                return Err(GitCacheError::Conflict(format!(
                    "generation chain for `{repo}` contains a cycle at `{current}`"
                )));
            }
            let manifest = if current == generation {
                if let Some(pending) = self.manifests().pending_generation(repo, current).await? {
                    pending.generation
                } else {
                    self.get_generation_manifest(repo, current)
                        .await?
                        .ok_or_else(|| {
                            GitCacheError::NotFound(format!(
                                "generation manifest `{current}` not found"
                            ))
                        })?
                }
            } else {
                self.get_generation_manifest(repo, current)
                    .await?
                    .ok_or_else(|| {
                        GitCacheError::NotFound(format!(
                            "generation manifest `{current}` not found"
                        ))
                    })?
            };
            next = manifest.parent_generation;
            chain.push(manifest);
        }
        Ok(chain)
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

    pub(super) async fn compact_generation_chain_inner(
        &self,
        repo: &RepoKey,
        threshold: usize,
        dry_run: bool,
    ) -> CoreResult<Option<CompactionReport>> {
        let Some(head) = self.manifests().repo_head(repo).await? else {
            return Ok(None);
        };
        let chain = self.generation_chain(repo, head.generation).await?;
        if chain.len() <= threshold {
            return Ok(None);
        }

        let old_generations: Vec<GenerationId> =
            chain.iter().map(|manifest| manifest.generation).collect();
        let old_generation_set: HashSet<GenerationId> = old_generations.iter().copied().collect();
        let all_old_generation_bytes = self
            .bundle_bytes_for_generations(repo, &old_generations)
            .await?;
        let new_generation = GenerationId::new();
        if dry_run {
            return Ok(Some(CompactionReport {
                repo: repo.clone(),
                old_chain_depth: chain.len(),
                old_generations,
                new_generation,
                bytes_reclaimed: all_old_generation_bytes,
            }));
        }

        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        Box::pin(self.verify_generation_with_semaphore(repo.clone(), head.generation, false))
            .await?;
        Box::pin(self.hydrate_generation(repo, &repo_dir, head.generation)).await?;
        let reservation = self.state.disk.reserve(1024 * 1024 * 64).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let bundle_path = temp_path.join("compacted.bundle");
        self.state
            .git
            .bundle_create_all(&repo_dir, &bundle_path)
            .await?;

        let now = Utc::now();
        let bundle_key = bundle_key(repo, new_generation);
        let commits = commits_from_chain(chain.iter().rev());
        let generation_manifest = GenerationManifest {
            repo: repo.clone(),
            generation: new_generation,
            bundle_key,
            parent_generation: None,
            created_at: now,
            commits: commits.clone(),
        };
        let new_head = RepoGenerationHead {
            repo: repo.clone(),
            generation: new_generation,
            tip_commits: head.tip_commits.clone(),
            updated_at: now,
        };
        GenerationPublish::new(generation_manifest.clone())
            .publish_pending_bundle_file(&*self.state.store, &bundle_path, new_head.clone(), None)
            .await?;
        Box::pin(self.verify_generation_with_semaphore(repo.clone(), new_generation, false))
            .await?;

        self.repoint_manifests_after_compaction(repo, &old_generation_set, new_generation)
            .await?;
        for commit in commits {
            let manifest = CommitManifest {
                repo: repo.clone(),
                commit,
                generation: new_generation,
                complete: true,
                verified_at: now,
            };
            self.manifests().write_commit(&manifest).await?;
        }
        self.manifests().write_repo_head(&new_head).await?;
        let retained_generations = self
            .old_generations_needed_by_pending_publishes(repo, &old_generation_set)
            .await?;
        let delete_generations = old_generations
            .iter()
            .copied()
            .filter(|generation| !retained_generations.contains(generation))
            .collect::<Vec<_>>();
        let bytes_reclaimed = self
            .bundle_bytes_for_generations(repo, &delete_generations)
            .await?;
        self.delete_old_generations(repo, &delete_generations)
            .await?;

        reservation.release().await?;

        Ok(Some(CompactionReport {
            repo: repo.clone(),
            old_chain_depth: old_generations.len(),
            old_generations,
            new_generation,
            bytes_reclaimed,
        }))
    }

    /// The repo's current generation bundle chain, oldest (root) first, for
    /// protocol-v2 `bundle-uri` advertisement. Empty when the repo has no
    /// published generations.
    pub async fn bundle_uri_generation_chain(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<Vec<GenerationManifest>> {
        let Some(head) = self.manifests().repo_head(repo).await? else {
            return Ok(Vec::new());
        };
        let mut chain = self.generation_chain(repo, head.generation).await?;
        chain.reverse();
        Ok(chain)
    }

    pub(super) async fn generation_chain(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Vec<GenerationManifest>> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(generation);
        while let Some(current) = next {
            if !seen.insert(current) {
                return Err(GitCacheError::Conflict(format!(
                    "generation chain for `{repo}` contains a cycle at `{current}`"
                )));
            }
            let manifest = self
                .get_generation_manifest(repo, current)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!("generation manifest `{current}` not found"))
                })?;
            next = manifest.parent_generation;
            chain.push(manifest);
        }
        Ok(chain)
    }

    pub(super) async fn old_generations_needed_by_pending_publishes(
        &self,
        repo: &RepoKey,
        old_generations: &HashSet<GenerationId>,
    ) -> CoreResult<HashSet<GenerationId>> {
        let keys = self
            .state
            .store
            .list_prefix(
                &format!("{PENDING_GENERATION_PREFIX}{repo}/"),
                Some(MAX_PENDING_GENERATION_SCAN_KEYS),
            )
            .await?;
        if keys.len() >= MAX_PENDING_GENERATION_SCAN_KEYS {
            return Err(GitCacheError::Conflict(format!(
                "too many pending generations for `{repo}` to compact safely"
            )));
        }

        let mut needed = HashSet::new();
        for key in keys {
            let Some((pending_repo, generation)) = pending_generation_from_key(&key)? else {
                continue;
            };
            if pending_repo != *repo {
                continue;
            }
            let Some(pending) = self
                .manifests()
                .pending_generation(repo, generation)
                .await?
            else {
                continue;
            };

            let mut seen = HashSet::new();
            let mut next = pending.generation.parent_generation;
            while let Some(current) = next {
                if !seen.insert(current) {
                    return Err(GitCacheError::Conflict(format!(
                        "pending generation chain for `{repo}` contains a cycle at `{current}`"
                    )));
                }
                if old_generations.contains(&current) {
                    needed.insert(current);
                }
                let Some(manifest) = self.get_generation_manifest(repo, current).await? else {
                    if old_generations.contains(&current) {
                        warn!(%repo, generation = %current, pending_generation = %generation, "pending generation references missing old generation manifest");
                    }
                    break;
                };
                next = manifest.parent_generation;
            }
        }

        Ok(needed)
    }

    pub(super) async fn bundle_bytes_for_generations(
        &self,
        repo: &RepoKey,
        generations: &[GenerationId],
    ) -> CoreResult<u64> {
        let mut total = 0_u64;
        for generation in generations {
            let key = bundle_key(repo, *generation);
            if let Some(meta) = self.state.store.head(&key).await? {
                total = total.saturating_add(meta.len);
            }
        }
        Ok(total)
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

    pub(super) async fn delete_old_generations(
        &self,
        repo: &RepoKey,
        generations: &[GenerationId],
    ) -> CoreResult<()> {
        for generation in generations {
            self.state
                .store
                .delete(&bundle_key(repo, *generation))
                .await?;
            self.state
                .store
                .delete(&generation_manifest_key(repo, *generation))
                .await?;
            self.state
                .store
                .delete(&verified_generation_manifest_key(repo, *generation))
                .await?;
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

pub fn bundle_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!(
        "repos/{}/generations/{}/base.bundle",
        repo.as_str(),
        generation
    )
}

pub(super) fn pending_generation_from_key(
    key: &str,
) -> CoreResult<Option<(RepoKey, GenerationId)>> {
    let Some(rest) = key.strip_prefix(PENDING_GENERATION_PREFIX) else {
        return Ok(None);
    };
    let Some((repo_part, generation_file)) = rest.rsplit_once('/') else {
        return Ok(None);
    };
    let Some(generation) = generation_file.strip_suffix(".json") else {
        return Ok(None);
    };
    let repo = RepoKey::parse(repo_part)?;
    let generation = GenerationId(uuid::Uuid::parse_str(generation).map_err(|err| {
        GitCacheError::Validation(format!(
            "invalid pending generation id `{generation}`: {err}"
        ))
    })?);
    Ok(Some((repo, generation)))
}

pub(super) fn verified_generation_manifest(
    generation: &GenerationManifest,
    bundle_len: u64,
    bundle_sha256: String,
    verified_at: chrono::DateTime<Utc>,
    tip_commits: Vec<CommitSha>,
) -> VerifiedGenerationManifest {
    VerifiedGenerationManifest {
        schema_version: VERIFIED_GENERATION_SCHEMA_VERSION,
        repo: generation.repo.clone(),
        generation: generation.generation,
        bundle_key: generation.bundle_key.clone(),
        bundle_len,
        bundle_sha256,
        parent_generation: generation.parent_generation,
        created_at: generation.created_at,
        verified_at,
        verifier_version: VERIFIED_GENERATION_VERIFIER_VERSION,
        git_version: "unknown".to_string(),
        fsck_mode: VERIFIED_GENERATION_FSCK_MODE.to_string(),
        commits: generation.commits.clone(),
        tip_commits,
    }
}

pub(super) fn validate_verified_generation(
    generation: &GenerationManifest,
    verification: &VerifiedGenerationManifest,
    object_len: u64,
) -> CoreResult<()> {
    if verification.schema_version != VERIFIED_GENERATION_SCHEMA_VERSION {
        return Err(GitCacheError::Validation(format!(
            "verified generation `{}` has unsupported schema version {}",
            generation.generation, verification.schema_version
        )));
    }
    if verification.repo != generation.repo
        || verification.generation != generation.generation
        || verification.bundle_key != generation.bundle_key
        || verification.parent_generation != generation.parent_generation
    {
        return Err(GitCacheError::Validation(format!(
            "verified generation manifest does not match generation `{}`",
            generation.generation
        )));
    }
    if verification.bundle_len == 0 {
        return Err(GitCacheError::Validation(format!(
            "verified generation `{}` has empty bundle",
            generation.generation
        )));
    }
    if object_len != verification.bundle_len {
        return Err(GitCacheError::Validation(format!(
            "bundle `{}` length mismatch: expected {}, got {}",
            generation.bundle_key, verification.bundle_len, object_len
        )));
    }
    validate_sha256_hex(&verification.bundle_sha256)?;
    Ok(())
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

pub(super) fn validate_sha256_hex(value: &str) -> CoreResult<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitCacheError::Validation(format!(
            "invalid sha256 digest `{value}`"
        )));
    }
    Ok(())
}

pub(super) fn push_unique_commit(commits: &mut Vec<CommitSha>, commit: CommitSha) {
    if !commits.iter().any(|existing| existing == &commit) {
        commits.push(commit);
    }
}

pub(super) fn commits_from_chain<'a>(
    chain: impl IntoIterator<Item = &'a GenerationManifest>,
) -> Vec<CommitSha> {
    let mut commits = Vec::new();
    for manifest in chain {
        for commit in &manifest.commits {
            push_unique_commit(&mut commits, commit.clone());
        }
    }
    commits
}
