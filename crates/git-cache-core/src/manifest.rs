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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackKind {
    Base,
    Delta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackInfo {
    pub key: String,
    pub len: u64,
    pub sha256: String,
    pub kind: PackKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationManifest {
    pub repo: RepoKey,
    pub generation: GenerationId,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub verified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub packs: Vec<PackInfo>,
    #[serde(default)]
    pub refs: std::collections::BTreeMap<String, CommitSha>,
    #[serde(default)]
    pub head_ref: Option<String>,
    #[serde(default)]
    pub commits: Vec<CommitSha>,
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
