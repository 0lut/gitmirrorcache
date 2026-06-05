use super::*;

#[derive(Debug, Clone)]
pub(super) struct RepoAccessContext {
    pub repo: RepoKey,
    pub upstream_auth: UpstreamAuth,
    pub proof: RepoAccessProof,
}

#[derive(Debug, Clone)]
pub(super) enum RepoAccessProof {
    PublicRef {
        ref_name: String,
        commit: CommitSha,
    },
    PublicCommit {
        commit: CommitSha,
    },
    AuthenticatedRef {
        ref_name: String,
        commit: CommitSha,
    },
    AuthenticatedCommit {
        commit: CommitSha,
        reachable_from: ReachabilitySelector,
        ref_name: String,
        tip: CommitSha,
    },
}

impl RepoAccessContext {
    pub fn public_ref(repo: RepoKey, ref_name: String, commit: CommitSha) -> Self {
        Self {
            repo,
            upstream_auth: UpstreamAuth::Anonymous,
            proof: RepoAccessProof::PublicRef { ref_name, commit },
        }
    }

    pub fn public_commit(repo: RepoKey, commit: CommitSha) -> Self {
        Self {
            repo,
            upstream_auth: UpstreamAuth::Anonymous,
            proof: RepoAccessProof::PublicCommit { commit },
        }
    }

    pub fn authenticated_ref(
        repo: RepoKey,
        upstream_auth: UpstreamAuth,
        ref_name: String,
        commit: CommitSha,
    ) -> Self {
        Self {
            repo,
            upstream_auth,
            proof: RepoAccessProof::AuthenticatedRef { ref_name, commit },
        }
    }

    pub fn authenticated_commit(
        repo: RepoKey,
        upstream_auth: UpstreamAuth,
        commit: CommitSha,
        reachable_from: ReachabilitySelector,
        ref_name: String,
        tip: CommitSha,
    ) -> Self {
        Self {
            repo,
            upstream_auth,
            proof: RepoAccessProof::AuthenticatedCommit {
                commit,
                reachable_from,
                ref_name,
                tip,
            },
        }
    }

    pub fn is_authenticated(&self) -> bool {
        matches!(
            self.proof,
            RepoAccessProof::AuthenticatedRef { .. } | RepoAccessProof::AuthenticatedCommit { .. }
        )
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

    pub fn session_authorized_refs(&self) -> Vec<String> {
        match &self.proof {
            RepoAccessProof::AuthenticatedRef { ref_name, .. }
            | RepoAccessProof::AuthenticatedCommit {
                ref_name,
                reachable_from: _,
                tip: _,
                ..
            } => {
                vec![ref_name.clone()]
            }
            RepoAccessProof::PublicRef { .. } | RepoAccessProof::PublicCommit { .. } => Vec::new(),
        }
    }

    pub fn proof_commit(&self) -> &CommitSha {
        match &self.proof {
            RepoAccessProof::PublicRef { commit, .. }
            | RepoAccessProof::PublicCommit { commit }
            | RepoAccessProof::AuthenticatedRef { commit, .. }
            | RepoAccessProof::AuthenticatedCommit { commit, .. } => commit,
        }
    }

    pub fn proof_ref_name(&self) -> Option<&str> {
        match &self.proof {
            RepoAccessProof::PublicRef { ref_name, .. }
            | RepoAccessProof::AuthenticatedRef { ref_name, .. }
            | RepoAccessProof::AuthenticatedCommit { ref_name, .. } => Some(ref_name.as_str()),
            RepoAccessProof::PublicCommit { .. } => None,
        }
    }

    pub fn reachability_proof(&self) -> Option<(&ReachabilitySelector, &CommitSha)> {
        match &self.proof {
            RepoAccessProof::AuthenticatedCommit {
                reachable_from,
                tip,
                ..
            } => Some((reachable_from, tip)),
            RepoAccessProof::PublicRef { .. }
            | RepoAccessProof::PublicCommit { .. }
            | RepoAccessProof::AuthenticatedRef { .. } => None,
        }
    }
}
