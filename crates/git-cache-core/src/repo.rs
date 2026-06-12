use crate::error::{GitCacheError, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoKey(String);

impl RepoKey {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_repo_key(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn host(&self) -> &str {
        self.segments()[0]
    }

    pub fn owner(&self) -> &str {
        self.segments()[1]
    }

    pub fn name(&self) -> &str {
        self.segments()[2]
    }

    pub fn local_bare_path(&self) -> String {
        format!("{}/{}/{}.git", self.host(), self.owner(), self.name())
    }

    fn segments(&self) -> Vec<&str> {
        self.0.split('/').collect()
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RepoKey {
    type Err = GitCacheError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for RepoKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_commit_sha(&value)?;
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for CommitSha {
    type Err = GitCacheError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for CommitSha {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CommitSha {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShortCommitSha(String);

impl ShortCommitSha {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_short_commit_sha(&value)?;
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ShortCommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ShortCommitSha {
    type Err = GitCacheError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for ShortCommitSha {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ShortCommitSha {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

fn validate_repo_key(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitCacheError::Validation("repo key is empty".into()));
    }

    if value.starts_with('/') || value.contains('\\') {
        return Err(GitCacheError::Validation(format!(
            "repo key `{value}` must be relative and use forward slashes"
        )));
    }

    let segments: Vec<_> = value.split('/').collect();
    if segments.len() != 3 {
        return Err(GitCacheError::Validation(format!(
            "repo key `{value}` must look like host/owner/repo"
        )));
    }

    for segment in segments {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(GitCacheError::Validation(format!(
                "repo key `{value}` contains an invalid path segment"
            )));
        }

        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(GitCacheError::Validation(format!(
                "repo key `{value}` contains unsupported characters"
            )));
        }
    }

    Ok(())
}

fn validate_commit_sha(value: &str) -> Result<()> {
    let valid_len = matches!(value.len(), 40 | 64);
    if !valid_len || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitCacheError::Validation(format!(
            "commit `{value}` must be a full 40 or 64 character hex SHA"
        )));
    }
    Ok(())
}

fn validate_short_commit_sha(value: &str) -> Result<()> {
    if !(4..64).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitCacheError::Validation(format!(
            "short commit `{value}` must be 4-63 hex characters"
        )));
    }

    if matches!(value.len(), 40 | 64) {
        return Err(GitCacheError::Validation(format!(
            "short commit `{value}` must be abbreviated; use `commit` for full object IDs"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
