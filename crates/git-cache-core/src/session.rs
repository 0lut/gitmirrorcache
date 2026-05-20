use crate::error::{GitCacheError, Result};
use crate::repo::{CommitSha, RepoKey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(Self(Uuid::parse_str(value).map_err(|err| {
            GitCacheError::Validation(format!("invalid session id `{value}`: {err}"))
        })?))
    }

    pub fn synthetic_ref(&self) -> String {
        format!("refs/cache/sessions/{}", self.0)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = GitCacheError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionManifest {
    pub id: SessionId,
    pub repo: RepoKey,
    pub commit: CommitSha,
    pub synthetic_ref: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
