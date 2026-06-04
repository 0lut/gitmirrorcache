use crate::{BranchName, CommitSha, GitCacheError, RepoKey, Result, Selector, ShortCommitSha};
use async_trait::async_trait;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    Cron,
    ReadThrough,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateTarget {
    Branch(BranchName),
    DefaultBranch,
    Commit(CommitSha),
    ShortCommit(ShortCommitSha),
    Ref(String),
}

impl UpdateTarget {
    pub fn from_selector(selector: &Selector) -> Self {
        match selector {
            Selector::Branch(branch) => Self::Branch(branch.clone()),
            Selector::DefaultBranch => Self::DefaultBranch,
            Selector::Commit(commit) => Self::Commit(commit.clone()),
            Selector::CommitReachableFrom { commit, .. } => Self::Commit(commit.clone()),
            Selector::ShortCommit(commit) => Self::ShortCommit(commit.clone()),
        }
    }

    pub fn from_event_ref(ref_name: impl Into<String>) -> Result<Self> {
        let ref_name = ref_name.into();
        validate_event_ref(&ref_name)?;

        if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
            return Ok(Self::Branch(BranchName::parse(branch)?));
        }

        Ok(Self::Ref(ref_name))
    }
}

impl Hash for UpdateTarget {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Branch(branch) => {
                0_u8.hash(state);
                branch.as_str().hash(state);
            }
            Self::DefaultBranch => {
                1_u8.hash(state);
            }
            Self::Commit(commit) => {
                2_u8.hash(state);
                commit.as_str().hash(state);
            }
            Self::ShortCommit(commit) => {
                3_u8.hash(state);
                commit.as_str().hash(state);
            }
            Self::Ref(ref_name) => {
                4_u8.hash(state);
                ref_name.hash(state);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpdateKey {
    pub repo: RepoKey,
    pub target: UpdateTarget,
}

impl UpdateKey {
    pub fn new(repo: RepoKey, target: UpdateTarget) -> Self {
        Self { repo, target }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRequest {
    pub repo: RepoKey,
    pub target: UpdateTarget,
    pub source: UpdateSource,
}

impl UpdateRequest {
    pub fn key(&self) -> UpdateKey {
        UpdateKey::new(self.repo.clone(), self.target.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDisposition {
    Updated,
    LeaseBusy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub key: UpdateKey,
    pub source: UpdateSource,
    pub disposition: UpdateDisposition,
}

impl UpdateOutcome {
    pub fn updated(request: &UpdateRequest) -> Self {
        Self {
            key: request.key(),
            source: request.source,
            disposition: UpdateDisposition::Updated,
        }
    }

    pub fn lease_busy(request: &UpdateRequest) -> Self {
        Self {
            key: request.key(),
            source: request.source,
            disposition: UpdateDisposition::LeaseBusy,
        }
    }
}

#[async_trait]
pub trait UpdateExecutor: Send + Sync {
    async fn update(&self, request: UpdateRequest) -> Result<()>;
}

pub fn validate_event_ref(ref_name: &str) -> Result<()> {
    if ref_name.is_empty() {
        return Err(GitCacheError::Validation("event ref is empty".into()));
    }

    if ref_name.bytes().any(|byte| byte.is_ascii_control())
        || ref_name.contains('\\')
        || ref_name.contains("..")
        || ref_name.ends_with(".lock")
    {
        return Err(GitCacheError::Validation(format!(
            "event ref `{ref_name}` is not safe to process"
        )));
    }

    Ok(())
}
