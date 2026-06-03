use crate::repo::{CommitSha, RepoKey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GenerationId(pub Uuid);

impl GenerationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for GenerationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
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
pub struct VerifiedGenerationManifest {
    pub schema_version: u32,
    pub repo: RepoKey,
    pub generation: GenerationId,
    pub bundle_key: String,
    pub bundle_len: u64,
    pub bundle_sha256: String,
    #[serde(default)]
    pub parent_generation: Option<GenerationId>,
    pub created_at: DateTime<Utc>,
    pub verified_at: DateTime<Utc>,
    pub verifier_version: u32,
    pub git_version: String,
    pub fsck_mode: String,
    #[serde(default)]
    pub commits: Vec<CommitSha>,
    #[serde(default)]
    pub tip_commits: Vec<CommitSha>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGenerationHead {
    pub repo: RepoKey,
    pub generation: GenerationId,
    pub tip_commits: Vec<CommitSha>,
    pub updated_at: DateTime<Utc>,
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
