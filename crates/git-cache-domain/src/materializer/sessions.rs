use super::access::RepoAccessContext;
use super::util::hex_lower;
use super::*;

impl Materializer {
    pub async fn create_session(
        &self,
        repo: RepoKey,
        commit: CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeResponse> {
        let access = RepoAccessContext::public_commit(repo, commit.clone());
        self.create_session_from_access(access, commit, source)
            .await
    }

    pub(super) async fn create_session_from_access(
        &self,
        access: RepoAccessContext,
        commit: CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeResponse> {
        debug_assert_eq!(access.proof_commit(), &commit);
        debug_assert_eq!(
            access.upstream_auth.is_authenticated(),
            access.is_authenticated()
        );
        debug!(
            repo = %access.repo,
            %commit,
            authenticated = access.is_authenticated(),
            ref_name = ?access.proof_ref_name(),
            "creating session from repo access proof"
        );
        self.create_session_with_access(access, commit, source)
            .await
    }

    async fn create_session_with_access(
        &self,
        access: RepoAccessContext,
        commit: CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeResponse> {
        let repo = access.repo.clone();
        let protected = access.is_authenticated();
        let session_id = SessionId::new();
        let synthetic_ref = session_id.synthetic_ref();
        let now = Utc::now();
        let expires_at =
            now + ChronoDuration::seconds(self.state.config.session_ttl_seconds as i64);
        let (protection, session_token) = if protected {
            let session_token = new_session_token();
            (
                SessionProtection::BearerToken {
                    token_hash: hash_session_token(&session_token),
                    authorized_commits: vec![commit.clone()],
                    authorized_refs: access.session_authorized_refs(),
                },
                Some(session_token),
            )
        } else {
            (SessionProtection::Public, None)
        };
        let manifest = SessionManifest {
            id: session_id,
            repo: repo.clone(),
            commit: commit.clone(),
            synthetic_ref: synthetic_ref.clone(),
            created_at: now,
            expires_at,
            protection,
        };

        let repo_dir = self.ensure_repo_dir(&repo).await?;
        if !self.commit_ready_for_serving(&repo_dir, &commit).await {
            let kind = if protected {
                "protected session"
            } else {
                "session"
            };
            return Err(GitCacheError::NotFound(format!(
                "cannot create {kind} for missing or incomplete local commit `{commit}`"
            )));
        }

        self.prepare_session_repo(&manifest, &repo_dir).await?;
        self.manifests().write_session(&manifest).await?;

        Ok(MaterializeResponse {
            repo: repo.clone(),
            commit,
            source,
            verified_at: now,
            git_url: format!(
                "{}/git/session/{}/{}.git",
                self.state.config.public_base_url.trim_end_matches('/'),
                session_id,
                repo.as_str()
            ),
            ref_name: synthetic_ref,
            session_token,
            expires_at,
        })
    }

    pub async fn session_repo_from_manifest(
        &self,
        repo: &RepoKey,
        session_id: SessionId,
        presented_token: Option<&str>,
    ) -> CoreResult<PathBuf> {
        let manifest: SessionManifest = self
            .get_session_manifest(repo, session_id)
            .await?
            .ok_or_else(|| GitCacheError::NotFound(format!("session `{session_id}` not found")))?;

        if manifest.expires_at < Utc::now() {
            return Err(GitCacheError::NotFound(format!(
                "session `{session_id}` expired"
            )));
        }

        if &manifest.repo != repo {
            return Err(GitCacheError::Validation(format!(
                "session `{session_id}` does not belong to repo `{repo}`"
            )));
        }

        match &manifest.protection {
            SessionProtection::Public => {}
            SessionProtection::BearerToken { token_hash, .. } => {
                let Some(presented_token) = presented_token else {
                    return Err(GitCacheError::Unauthorized(
                        "session bearer token is required".into(),
                    ));
                };
                if !constant_time_eq(
                    token_hash.as_bytes(),
                    hash_session_token(presented_token).as_bytes(),
                ) {
                    return Err(GitCacheError::Unauthorized(
                        "session bearer token is invalid".into(),
                    ));
                }
            }
        }

        let repo_dir = self.ensure_repo_dir(&manifest.repo).await?;
        if !self
            .commit_ready_for_serving(&repo_dir, &manifest.commit)
            .await
        {
            let commit_manifest = self
                .get_commit_manifest(&manifest.repo, &manifest.commit)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!(
                        "session commit `{}` is missing from manifests",
                        manifest.commit
                    ))
                })?;
            self.hydrate_commit(&commit_manifest).await?;
        }

        self.prepare_session_repo(&manifest, &repo_dir).await?;
        Ok(self.session_repo_path(session_id))
    }

    pub async fn cleanup_expired_sessions(&self) -> CoreResult<SessionCleanupReport> {
        let keys = self.state.store.list_prefix("repos/", None).await?;
        let session_keys: Vec<String> = keys
            .into_iter()
            .filter(|k| k.contains("/manifests/sessions/") && k.ends_with(".json"))
            .collect();

        let mut sessions_removed: usize = 0;
        let mut errors: Vec<String> = Vec::new();
        let now = Utc::now();

        for key in session_keys {
            let manifest: Option<SessionManifest> = match read_json(&*self.state.store, &key).await
            {
                Ok(m) => m,
                Err(err) => {
                    errors.push(format!("failed to read `{key}`: {err}"));
                    continue;
                }
            };

            let Some(manifest) = manifest else {
                continue;
            };

            if manifest.expires_at >= now {
                continue;
            }

            let session_dir = self.session_repo_path(manifest.id);
            if session_dir.exists() {
                let dir = session_dir.clone();
                if let Err(err) = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir))
                    .await
                    .map_err(|err| GitCacheError::Io(std::io::Error::other(err)))
                    .and_then(|r| r.map_err(GitCacheError::Io))
                {
                    errors.push(format!(
                        "failed to remove session dir `{}`: {err}",
                        session_dir.display()
                    ));
                    continue;
                }
            }

            if let Err(err) = self.state.store.delete(&key).await {
                errors.push(format!("failed to delete manifest `{key}`: {err}"));
                continue;
            }

            sessions_removed += 1;
            debug!(session_id = %manifest.id, "cleaned up expired session");
        }

        Ok(SessionCleanupReport {
            sessions_removed,
            errors,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCleanupReport {
    pub sessions_removed: usize,
    pub errors: Vec<String>,
}

pub(super) fn new_session_token() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    format!("gcs_{}", hex_lower(&bytes))
}

pub(super) fn hash_session_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex_lower(&hasher.finalize())
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}
