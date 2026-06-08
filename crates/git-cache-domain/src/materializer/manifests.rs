use super::*;

pub(super) struct ManifestStore<'a> {
    state: &'a AppState,
}

impl<'a> ManifestStore<'a> {
    pub(super) fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    pub(super) async fn commit(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CoreResult<Option<CommitManifest>> {
        read_commit_manifest(&*self.state.store, repo, commit).await
    }

    pub(super) async fn write_commit(&self, manifest: &CommitManifest) -> CoreResult<()> {
        write_commit_manifest(&*self.state.store, manifest).await
    }

    pub(super) async fn commit_by_key(&self, key: &str) -> CoreResult<Option<CommitManifest>> {
        read_json::<_, CommitManifest>(&*self.state.store, key).await
    }

    pub(super) async fn generation(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Option<GenerationManifest>> {
        read_generation_manifest(&*self.state.store, repo, generation).await
    }

    pub(super) async fn session(
        &self,
        repo: &RepoKey,
        session_id: SessionId,
    ) -> CoreResult<Option<SessionManifest>> {
        read_session_manifest(&*self.state.store, repo, session_id).await
    }

    pub(super) async fn write_session(&self, manifest: &SessionManifest) -> CoreResult<()> {
        write_session_manifest(&*self.state.store, manifest).await
    }

    pub(super) async fn ref_by_key(&self, key: &str) -> CoreResult<Option<RefManifest>> {
        read_json::<_, RefManifest>(&*self.state.store, key).await
    }

    #[cfg(test)]
    pub(super) async fn ref_manifest(
        &self,
        repo: &RepoKey,
        ref_name: &str,
    ) -> CoreResult<Option<RefManifest>> {
        git_cache_objectstore::read_ref_manifest(&*self.state.store, repo, ref_name).await
    }

    pub(super) async fn write_ref(&self, manifest: &RefManifest) -> CoreResult<()> {
        write_ref_manifest(&*self.state.store, manifest).await
    }

    pub(super) async fn write_default_ref(
        &self,
        repo: &RepoKey,
        manifest: &RefManifest,
    ) -> CoreResult<()> {
        self.write_ref(manifest).await?;
        write_json(&*self.state.store, &default_manifest_key(repo), manifest).await
    }

    pub(super) async fn repo_head(&self, repo: &RepoKey) -> CoreResult<Option<RepoGenerationHead>> {
        read_repo_generation_head(&*self.state.store, repo).await
    }

    pub(super) async fn write_repo_head(&self, head: &RepoGenerationHead) -> CoreResult<()> {
        write_repo_generation_head(&*self.state.store, head).await
    }

    pub(super) async fn pending_generation(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Option<git_cache_objectstore::PendingGenerationPublish>> {
        read_pending_generation_publish(&*self.state.store, repo, generation).await
    }

    pub(super) async fn verified_generation(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Option<VerifiedGenerationManifest>> {
        read_verified_generation_manifest(&*self.state.store, repo, generation).await
    }

    pub(super) async fn write_verified_if_absent_or_matches(
        &self,
        manifest: &VerifiedGenerationManifest,
    ) -> CoreResult<bool> {
        write_verified_generation_manifest_if_absent_or_matches(&*self.state.store, manifest).await
    }
}

impl Materializer {
    pub(super) fn manifests(&self) -> ManifestStore<'_> {
        ManifestStore::new(self.state.as_ref())
    }
}
