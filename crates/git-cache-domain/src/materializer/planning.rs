use super::generations::push_unique_commit;
use super::*;

#[derive(Debug, Clone)]
struct MaterializePlan {
    repo: RepoKey,
    access: RepoAccess,
    target: MaterializeTarget,
}

#[derive(Debug, Clone)]
enum MaterializeTarget {
    Commit {
        commit: CommitSha,
        source: MaterializeSource,
    },
    BranchTip {
        branch: BranchName,
        commit: CommitSha,
        default_branch: bool,
    },
    ReachableCommit {
        commit: CommitSha,
        branch: BranchName,
        tip: CommitSha,
    },
}

#[derive(Debug, Clone)]
struct ResolvePlan {
    repo: RepoKey,
    selector: Selector,
    target: ResolveTarget,
    access: RepoAccess,
}

#[derive(Debug, Clone)]
enum ResolveTarget {
    Commit { commit: CommitSha },
    ReachableCommit { commit: CommitSha, tip: CommitSha },
}

#[derive(Debug, Clone)]
enum RepoAccess {
    Public,
    Upstream { refs: Vec<String> },
}

impl RepoAccess {
    fn from_request(request: &MaterializeRequest, auth: &UpstreamAuth) -> CoreResult<Self> {
        if request.requires_upstream_auth() && !auth.is_authenticated() {
            return Err(GitCacheError::Unauthorized(
                "upstream authorization is required".into(),
            ));
        }
        if auth.is_authenticated() {
            Ok(Self::Upstream { refs: Vec::new() })
        } else {
            Ok(Self::Public)
        }
    }

    fn with_ref(self, ref_name: String) -> Self {
        match self {
            Self::Public => Self::Public,
            Self::Upstream { mut refs } => {
                refs.push(ref_name);
                Self::Upstream { refs }
            }
        }
    }

    fn is_upstream(&self) -> bool {
        matches!(self, Self::Upstream { .. })
    }

    fn cache_hit_source(&self) -> MaterializeSource {
        match self {
            Self::Public => MaterializeSource::CacheVerified,
            Self::Upstream { .. } => MaterializeSource::UpstreamAuthorizedCacheHit,
        }
    }

    fn fetched_source(&self) -> MaterializeSource {
        match self {
            Self::Public => MaterializeSource::UpstreamVerified,
            Self::Upstream { .. } => MaterializeSource::UpstreamAuthorizedFetched,
        }
    }
}

impl Materializer {
    pub async fn materialize(
        &self,
        request: MaterializeRequest,
    ) -> CoreResult<MaterializeResponse> {
        let plan = self.plan_materialize(request).await?;
        self.materialize_plan(plan).await
    }

    pub async fn materialize_after_upstream_validation(
        &self,
        request: MaterializeRequest,
    ) -> CoreResult<MaterializeResponse> {
        let plan = self
            .plan_materialize_after_upstream_validation(request)
            .await?;
        self.materialize_plan(plan).await
    }

    pub async fn resolve(&self, request: MaterializeRequest) -> CoreResult<ResolveResponse> {
        let plan = self.plan_resolve(request).await?;
        self.resolve_plan(plan).await
    }

    async fn plan_materialize(&self, request: MaterializeRequest) -> CoreResult<MaterializePlan> {
        self.validate_host(&request.repo)?;
        let access = RepoAccess::from_request(&request, &self.upstream_auth)?;
        if access.is_upstream() {
            return self.plan_upstream_materialize(request, access).await;
        }
        self.plan_public_materialize(request, access).await
    }

    async fn plan_materialize_after_upstream_validation(
        &self,
        request: MaterializeRequest,
    ) -> CoreResult<MaterializePlan> {
        self.validate_host(&request.repo)?;
        let access = RepoAccess::from_request(&request, &self.upstream_auth)?;
        if access.is_upstream() {
            return self.plan_upstream_materialize(request, access).await;
        }

        match request.selector {
            Selector::Branch(branch) => {
                let commit = self.local_branch_tip(&request.repo, &branch).await?;
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::Commit {
                        commit,
                        source: MaterializeSource::UpstreamVerified,
                    },
                })
            }
            Selector::DefaultBranch => {
                let branch = self.resolve_default_branch(&request.repo).await?;
                let commit = self.local_branch_tip(&request.repo, &branch).await?;
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::Commit {
                        commit,
                        source: MaterializeSource::UpstreamVerified,
                    },
                })
            }
            _ => self.plan_public_materialize(request, access).await,
        }
    }

    async fn plan_public_materialize(
        &self,
        request: MaterializeRequest,
        access: RepoAccess,
    ) -> CoreResult<MaterializePlan> {
        match request.selector {
            Selector::Commit(commit) | Selector::CommitReachableFrom { commit, .. } => {
                let source = self.ensure_public_commit(&request.repo, &commit).await?;
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::Commit { commit, source },
                })
            }
            Selector::ShortCommit(short_commit) => {
                let (commit, source) = self
                    .ensure_public_short_commit(&request.repo, short_commit)
                    .await?;
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::Commit { commit, source },
                })
            }
            Selector::Branch(branch) => {
                let (branch, commit) = self.resolve_branch_tip(&request.repo, branch).await?;
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::BranchTip {
                        branch,
                        commit,
                        default_branch: false,
                    },
                })
            }
            Selector::DefaultBranch => {
                let (branch, commit) = self.resolve_default_branch_tip(&request.repo).await?;
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::BranchTip {
                        branch,
                        commit,
                        default_branch: true,
                    },
                })
            }
        }
    }

    async fn plan_upstream_materialize(
        &self,
        request: MaterializeRequest,
        access: RepoAccess,
    ) -> CoreResult<MaterializePlan> {
        match request.selector {
            Selector::Branch(branch) => {
                let (branch, commit) = self.resolve_branch_tip(&request.repo, branch).await?;
                let access = access.with_ref(format!("refs/heads/{branch}"));
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::BranchTip {
                        branch,
                        commit,
                        default_branch: false,
                    },
                })
            }
            Selector::DefaultBranch => {
                let (branch, commit) = self.resolve_default_branch_tip(&request.repo).await?;
                let access = access.with_ref(format!("refs/heads/{branch}"));
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::BranchTip {
                        branch,
                        commit,
                        default_branch: true,
                    },
                })
            }
            Selector::CommitReachableFrom {
                commit,
                reachable_from,
            } => {
                let (branch, tip) = self
                    .resolve_reachability_tip(&request.repo, reachable_from)
                    .await?;
                let access = access.with_ref(format!("refs/heads/{branch}"));
                Ok(MaterializePlan {
                    repo: request.repo,
                    access,
                    target: MaterializeTarget::ReachableCommit {
                        commit,
                        branch,
                        tip,
                    },
                })
            }
            Selector::Commit(_) | Selector::ShortCommit(_) => Err(GitCacheError::Validation(
                "authenticated commit selectors require reachable_from context".into(),
            )),
        }
    }

    async fn plan_resolve(&self, request: MaterializeRequest) -> CoreResult<ResolvePlan> {
        self.validate_host(&request.repo)?;
        let access = RepoAccess::from_request(&request, &self.upstream_auth)?;
        if !access.is_upstream() {
            return Err(GitCacheError::Unsupported(
                "anonymous resolve still uses the materialize-compatible endpoint".into(),
            ));
        }

        let selector = request.selector.clone();
        match selector.clone() {
            Selector::Branch(branch) => {
                let (_, commit) = self.resolve_branch_tip(&request.repo, branch).await?;
                Ok(ResolvePlan {
                    repo: request.repo,
                    selector,
                    target: ResolveTarget::Commit { commit },
                    access,
                })
            }
            Selector::DefaultBranch => {
                let (_, commit) = self.resolve_default_branch_tip(&request.repo).await?;
                Ok(ResolvePlan {
                    repo: request.repo,
                    selector,
                    target: ResolveTarget::Commit { commit },
                    access,
                })
            }
            Selector::CommitReachableFrom {
                commit,
                reachable_from,
            } => {
                let (_, tip) = self
                    .resolve_reachability_tip(&request.repo, reachable_from)
                    .await?;
                Ok(ResolvePlan {
                    repo: request.repo,
                    selector,
                    target: ResolveTarget::ReachableCommit { commit, tip },
                    access,
                })
            }
            Selector::Commit(_) | Selector::ShortCommit(_) => Err(GitCacheError::Validation(
                "authenticated commit selectors require reachable_from context".into(),
            )),
        }
    }

    async fn materialize_plan(&self, plan: MaterializePlan) -> CoreResult<MaterializeResponse> {
        match plan.target {
            MaterializeTarget::Commit { commit, source } => {
                self.create_session_for_access(plan.repo, commit, source, plan.access)
                    .await
            }
            MaterializeTarget::BranchTip {
                branch,
                commit,
                default_branch,
            } => {
                let source = self
                    .ensure_branch_tip(&plan.repo, &branch, &commit, default_branch, &plan.access)
                    .await?;
                self.create_session_for_access(plan.repo, commit, source, plan.access)
                    .await
            }
            MaterializeTarget::ReachableCommit {
                commit,
                branch,
                tip,
            } => {
                let source = self
                    .ensure_reachable_commit(&plan.repo, &commit, &branch, &tip, &plan.access)
                    .await?;
                self.create_session_for_access(plan.repo, commit, source, plan.access)
                    .await
            }
        }
    }

    async fn resolve_plan(&self, plan: ResolvePlan) -> CoreResult<ResolveResponse> {
        let now = Utc::now();
        match plan.target {
            ResolveTarget::Commit { commit } => {
                let cache_available = self.cache_has_commit(&plan.repo, &commit).await?;
                Ok(ResolveResponse {
                    repo: plan.repo,
                    selector: plan.selector,
                    commit,
                    source: if cache_available {
                        plan.access.cache_hit_source()
                    } else {
                        plan.access.fetched_source()
                    },
                    cache_available,
                    authorized_at: now,
                })
            }
            ResolveTarget::ReachableCommit { commit, tip } => {
                let repo_dir = self.ensure_repo_dir(&plan.repo).await?;
                let cache_available = self.commit_exists(&repo_dir, &commit).await
                    && self.commit_exists(&repo_dir, &tip).await
                    && self.state.git.is_ancestor(&repo_dir, &commit, &tip).await?;
                if !cache_available {
                    return Err(GitCacheError::NotFound(format!(
                        "commit `{commit}` is not available with local reachability proof"
                    )));
                }
                Ok(ResolveResponse {
                    repo: plan.repo,
                    selector: plan.selector,
                    commit,
                    source: plan.access.cache_hit_source(),
                    cache_available,
                    authorized_at: now,
                })
            }
        }
    }

    async fn resolve_branch_tip(
        &self,
        repo: &RepoKey,
        branch: BranchName,
    ) -> CoreResult<(BranchName, CommitSha)> {
        let remote = self.upstream_url(repo)?;
        let ls = self.upstream_git(&remote)?.ls_remote_heads(&remote).await?;
        let sha = ls.refs.get(branch.as_str()).ok_or_else(|| {
            GitCacheError::NotFound(format!("branch `{branch}` was verified absent upstream"))
        })?;
        Ok((branch, CommitSha::parse(sha)?))
    }

    async fn resolve_default_branch_tip(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<(BranchName, CommitSha)> {
        let remote = self.upstream_url(repo)?;
        let ls = self.upstream_git(&remote)?.ls_remote_heads(&remote).await?;
        let branch = ls.default_branch.ok_or_else(|| {
            GitCacheError::UpstreamUnavailable("upstream did not advertise a symbolic HEAD".into())
        })?;
        let sha = ls.refs.get(&branch).ok_or_else(|| {
            GitCacheError::UpstreamUnavailable("upstream HEAD pointed at a missing branch".into())
        })?;
        Ok((BranchName::parse(branch)?, CommitSha::parse(sha)?))
    }

    async fn resolve_reachability_tip(
        &self,
        repo: &RepoKey,
        reachable_from: ReachabilitySelector,
    ) -> CoreResult<(BranchName, CommitSha)> {
        match reachable_from {
            ReachabilitySelector::Branch(branch) => self.resolve_branch_tip(repo, branch).await,
            ReachabilitySelector::DefaultBranch => self.resolve_default_branch_tip(repo).await,
        }
    }

    async fn local_branch_tip(&self, repo: &RepoKey, branch: &BranchName) -> CoreResult<CommitSha> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let local_ref = format!("refs/cache/upstream/heads/{}", branch.as_str());
        self.state
            .git
            .rev_parse(&repo_dir, &local_ref)
            .await
            .and_then(CommitSha::parse)
    }

    async fn ensure_branch_tip(
        &self,
        repo: &RepoKey,
        branch: &BranchName,
        commit: &CommitSha,
        default_branch: bool,
        access: &RepoAccess,
    ) -> CoreResult<MaterializeSource> {
        let repo_dir = self.ensure_repo_dir(repo).await?;

        if access.is_upstream() && self.cache_has_commit(repo, commit).await? {
            if let Some(manifest) = self.get_commit_manifest(repo, commit).await? {
                if manifest.complete {
                    self.hydrate_commit_in_repo(&repo_dir, &manifest).await?;
                }
            }
            if !self.commit_exists(&repo_dir, commit).await {
                return Err(GitCacheError::NotFound(format!(
                    "commit `{commit}` is marked cached but could not be hydrated"
                )));
            }
            return Ok(access.cache_hit_source());
        }

        self.ensure_branch_from_verified_tip(repo, branch, commit, default_branch)
            .await?;
        Ok(access.fetched_source())
    }

    async fn ensure_reachable_commit(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
        branch: &BranchName,
        tip: &CommitSha,
        access: &RepoAccess,
    ) -> CoreResult<MaterializeSource> {
        let branch_source = self
            .ensure_branch_tip(repo, branch, tip, false, access)
            .await?;
        let repo_dir = self.ensure_repo_dir(repo).await?;

        if !self.commit_exists(&repo_dir, commit).await {
            if let Some(manifest) = self.get_commit_manifest(repo, commit).await? {
                if manifest.complete {
                    self.hydrate_commit_in_repo(&repo_dir, &manifest).await?;
                }
            }
        }
        if !self.commit_exists(&repo_dir, commit).await {
            return Err(GitCacheError::NotFound(format!(
                "commit `{commit}` was not found after fetching authorized ref `{branch}`"
            )));
        }
        if !self.state.git.is_ancestor(&repo_dir, commit, tip).await? {
            return Err(GitCacheError::Forbidden(format!(
                "commit `{commit}` is not reachable from authorized ref `{branch}`"
            )));
        }

        if self.get_commit_manifest(repo, commit).await?.is_none() {
            self.publish_generation(repo, &repo_dir, commit, None, false)
                .await?;
        }

        let source = if branch_source == access.cache_hit_source() {
            access.cache_hit_source()
        } else {
            access.fetched_source()
        };

        Ok(source)
    }

    async fn create_session_for_access(
        &self,
        repo: RepoKey,
        commit: CommitSha,
        source: MaterializeSource,
        access: RepoAccess,
    ) -> CoreResult<MaterializeResponse> {
        match access {
            RepoAccess::Public => self.create_session(repo, commit, source).await,
            RepoAccess::Upstream { refs } => {
                self.create_protected_session(repo, commit, source, refs)
                    .await
            }
        }
    }

    async fn cache_has_commit(&self, repo: &RepoKey, commit: &CommitSha) -> CoreResult<bool> {
        if self
            .get_commit_manifest(repo, commit)
            .await?
            .is_some_and(|manifest| manifest.complete)
        {
            return Ok(true);
        }
        let repo_dir = self.ensure_repo_dir(repo).await?;
        Ok(self.commit_exists(&repo_dir, commit).await)
    }

    pub async fn materialize_short_commit(
        &self,
        repo: RepoKey,
        short_commit: ShortCommitSha,
    ) -> CoreResult<MaterializeResponse> {
        let (commit, source) = self.ensure_public_short_commit(&repo, short_commit).await?;
        self.create_session(repo, commit, source).await
    }

    pub async fn materialize_commit(
        &self,
        repo: RepoKey,
        commit: CommitSha,
    ) -> CoreResult<MaterializeResponse> {
        let source = self.ensure_public_commit(&repo, &commit).await?;
        self.create_session(repo, commit, source).await
    }

    async fn ensure_public_short_commit(
        &self,
        repo: &RepoKey,
        short_commit: ShortCommitSha,
    ) -> CoreResult<(CommitSha, MaterializeSource)> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        self.fetch_all_refs(repo, &repo_dir).await?;
        let commit = self
            .resolve_short_commit_from_upstream_refs(&repo_dir, &short_commit)
            .await?;
        self.publish_existing_local_commit(
            repo,
            &repo_dir,
            &commit,
            MaterializeSource::UpstreamVerified,
        )
        .await?;
        Ok((commit, MaterializeSource::UpstreamVerified))
    }

    async fn ensure_public_commit(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CoreResult<MaterializeSource> {
        if let Some(manifest) = self.get_commit_manifest(repo, commit).await? {
            if manifest.complete {
                self.hydrate_commit(&manifest).await?;
                return Ok(MaterializeSource::CacheVerified);
            }
        }

        let repo_dir = self.ensure_repo_dir(repo).await?;
        if self.commit_exists(&repo_dir, commit).await {
            if let Some(generation) = self
                .index_local_commit_from_known_generation(repo, &repo_dir, commit)
                .await?
            {
                debug!(%repo, %commit, %generation, "indexed exact commit from known generation");
                return Ok(MaterializeSource::CacheVerified);
            }
        }

        self.fetch_all_refs(repo, &repo_dir).await?;

        if !self.commit_exists(&repo_dir, commit).await {
            return Err(GitCacheError::NotFound(format!(
                "commit `{commit}` was not found after upstream verification"
            )));
        }

        if let Some(generation) = self
            .index_local_commit_from_known_generation(repo, &repo_dir, commit)
            .await?
        {
            debug!(%repo, %commit, %generation, "indexed exact commit from known generation after upstream fetch");
            return Ok(MaterializeSource::CacheVerified);
        }

        self.publish_existing_local_commit(
            repo,
            &repo_dir,
            commit,
            MaterializeSource::UpstreamVerified,
        )
        .await
    }

    async fn publish_existing_local_commit(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeSource> {
        let generation = self
            .publish_generation(repo, repo_dir, commit, None, false)
            .await?;
        debug!(%repo, %commit, %generation, "published generation for exact commit");
        Ok(source)
    }

    async fn index_local_commit_from_known_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> CoreResult<Option<GenerationId>> {
        let Some(head) = self.manifests().repo_head(repo).await? else {
            return Ok(None);
        };

        let mut checked_tips = Vec::new();
        for tip in head.tip_commits.iter().rev() {
            push_unique_commit(&mut checked_tips, tip.clone());
            if self
                .index_local_commit_if_ancestor_of_tip(repo, repo_dir, commit, tip, head.generation)
                .await?
            {
                return Ok(Some(head.generation));
            }
        }

        let candidate_tips = self
            .state
            .git
            .for_each_ref_containing_commit(
                repo_dir,
                commit,
                &["refs/cache/commits", "refs/cache/upstream/heads"],
            )
            .await?;

        for tip in candidate_tips {
            if checked_tips.iter().any(|checked| checked == &tip) {
                continue;
            }
            push_unique_commit(&mut checked_tips, tip.clone());
            let Some(tip_manifest) = self.get_commit_manifest(repo, &tip).await? else {
                continue;
            };
            if !tip_manifest.complete {
                continue;
            }
            self.index_local_commit(repo, commit, tip_manifest.generation)
                .await?;
            return Ok(Some(tip_manifest.generation));
        }

        Ok(None)
    }

    async fn index_local_commit_if_ancestor_of_tip(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        tip: &CommitSha,
        generation: GenerationId,
    ) -> CoreResult<bool> {
        let Some(tip_manifest) = self.get_commit_manifest(repo, tip).await? else {
            return Ok(false);
        };
        if !tip_manifest.complete || tip_manifest.generation != generation {
            return Ok(false);
        }
        if !self.commit_exists(repo_dir, tip).await {
            return Ok(false);
        }
        if !self.state.git.is_ancestor(repo_dir, commit, tip).await? {
            return Ok(false);
        }
        self.index_local_commit(repo, commit, generation).await?;
        Ok(true)
    }

    async fn index_local_commit(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let manifest = CommitManifest {
            repo: repo.clone(),
            commit: commit.clone(),
            generation,
            complete: true,
            verified_at: Utc::now(),
        };
        self.manifests().write_commit(&manifest).await?;
        Ok(())
    }

    pub async fn materialize_branch(
        &self,
        repo: RepoKey,
        branch: BranchName,
        _mode: RequestMode,
        default_branch: bool,
    ) -> CoreResult<MaterializeResponse> {
        let commit = self.ensure_branch(&repo, &branch, default_branch).await?;
        self.create_session(repo, commit, MaterializeSource::UpstreamVerified)
            .await
    }

    /// Fetch and publish a branch from upstream without creating a session.
    /// Returns the verified commit SHA.
    pub async fn ensure_branch(
        &self,
        repo: &RepoKey,
        branch: &BranchName,
        default_branch: bool,
    ) -> CoreResult<CommitSha> {
        self.validate_host(repo)?;
        let upstream_commit = self.ls_remote_branch(repo, branch).await?;
        self.ensure_branch_from_verified_tip(repo, branch, &upstream_commit, default_branch)
            .await
    }

    async fn ensure_branch_from_verified_tip(
        &self,
        repo: &RepoKey,
        branch: &BranchName,
        upstream_commit: &CommitSha,
        default_branch: bool,
    ) -> CoreResult<CommitSha> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let local_ref = format!("refs/cache/upstream/heads/{}", branch.as_str());
        let upstream_url = self.upstream_url(repo)?;
        self.upstream_git(&upstream_url)?
            .fetch_branch(&repo_dir, &upstream_url, branch.as_str(), &local_ref)
            .await?;

        let commit = self
            .state
            .git
            .rev_parse(&repo_dir, &local_ref)
            .await
            .and_then(CommitSha::parse)?;

        if commit != *upstream_commit {
            return Err(GitCacheError::Conflict(format!(
                "upstream branch `{branch}` moved during fetch: ls-remote={upstream_commit}, fetched={commit}"
            )));
        }

        if default_branch {
            self.state
                .git
                .symbolic_ref(
                    &repo_dir,
                    "HEAD",
                    &format!("refs/cache/upstream/heads/{branch}"),
                )
                .await?;
        }

        if !self.upstream_auth.is_authenticated() {
            self.state
                .git
                .update_ref(&repo_dir, &format!("refs/heads/{branch}"), commit.as_str())
                .await?;
            if default_branch {
                self.state
                    .git
                    .symbolic_ref(&repo_dir, "HEAD", &format!("refs/heads/{branch}"))
                    .await?;
            }
        }

        self.publish_generation(
            repo,
            &repo_dir,
            &commit,
            Some(branch.clone()),
            default_branch,
        )
        .await?;

        Ok(commit)
    }

    pub async fn materialize_default_branch(
        &self,
        repo: RepoKey,
        _mode: RequestMode,
    ) -> CoreResult<MaterializeResponse> {
        let commit = self.ensure_default_branch(&repo).await?;
        self.create_session(repo, commit, MaterializeSource::UpstreamVerified)
            .await
    }

    /// Resolve, fetch and publish the default branch without creating a session.
    /// Returns the verified commit SHA.
    pub async fn ensure_default_branch(&self, repo: &RepoKey) -> CoreResult<CommitSha> {
        self.validate_host(repo)?;
        let branch = self.resolve_default_branch(repo).await?;
        self.ensure_branch(repo, &branch, true).await
    }
}
