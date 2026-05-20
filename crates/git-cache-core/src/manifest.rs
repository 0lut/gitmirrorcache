use crate::repo::{CommitSha, RepoKey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GenerationId(pub u64);

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationManifest {
    pub repo: RepoKey,
    pub generation: GenerationId,
    pub bundle_key: String,
    #[serde(default)]
    pub parent_generation: Option<GenerationId>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub commits: Vec<CommitSha>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitManifest {
    pub repo: RepoKey,
    pub commit: CommitSha,
    pub generation: GenerationId,
    pub complete: bool,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefManifest {
    pub repo: RepoKey,
    pub ref_name: String,
    pub commit: CommitSha,
    pub generation: GenerationId,
    pub verified_at: DateTime<Utc>,
}
