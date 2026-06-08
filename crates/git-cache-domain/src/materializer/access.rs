use super::*;

#[derive(Debug, Clone)]
pub(super) struct RepoAccessContext {
    pub repo: RepoKey,
    authenticated: bool,
}

impl RepoAccessContext {
    pub fn public_ref(repo: RepoKey, _ref_name: String, _commit: CommitSha) -> Self {
        Self {
            repo,
            authenticated: false,
        }
    }

    pub fn public_commit(repo: RepoKey, _commit: CommitSha) -> Self {
        Self {
            repo,
            authenticated: false,
        }
    }

    pub fn authenticated_ref(
        repo: RepoKey,
        _upstream_auth: UpstreamAuth,
        _ref_name: String,
        _commit: CommitSha,
    ) -> Self {
        Self {
            repo,
            authenticated: true,
        }
    }

    pub fn authenticated_commit(
        repo: RepoKey,
        _upstream_auth: UpstreamAuth,
        _commit: CommitSha,
    ) -> Self {
        Self {
            repo,
            authenticated: true,
        }
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn cache_hit_source(&self) -> MaterializeSource {
        if self.is_authenticated() {
            MaterializeSource::UpstreamAuthorizedCacheHit
        } else {
            MaterializeSource::CacheVerified
        }
    }

    pub fn fetched_source(&self) -> MaterializeSource {
        if self.is_authenticated() {
            MaterializeSource::UpstreamAuthorizedFetched
        } else {
            MaterializeSource::UpstreamVerified
        }
    }
}
