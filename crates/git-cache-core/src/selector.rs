use crate::error::{GitCacheError, Result};
use crate::repo::{CommitSha, ShortCommitSha};
use serde::de::{Error as _, IgnoredAny};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BranchName(String);

impl BranchName {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_branch_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn ref_name(&self) -> String {
        format!("refs/heads/{}", self.0)
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BranchName {
    type Err = GitCacheError;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl Serialize for BranchName {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BranchName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Commit(CommitSha),
    ShortCommit(ShortCommitSha),
    Branch(BranchName),
    DefaultBranch,
}

impl Serialize for Selector {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Commit(commit) => map.serialize_entry("commit", commit)?,
            Self::ShortCommit(commit) => map.serialize_entry("short_commit", commit)?,
            Self::Branch(branch) => map.serialize_entry("branch", branch)?,
            Self::DefaultBranch => map.serialize_entry("default_branch", &true)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Selector {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireSelector {
            commit: Option<CommitSha>,
            reachable_from: Option<IgnoredAny>,
            short_commit: Option<ShortCommitSha>,
            branch: Option<BranchName>,
            #[serde(default)]
            default_branch: bool,
        }

        let wire = WireSelector::deserialize(deserializer)?;
        if wire.reachable_from.is_some() {
            return Err(D::Error::custom(
                "reachable_from selectors are not supported",
            ));
        }

        let selected = wire.commit.is_some() as u8
            + wire.short_commit.is_some() as u8
            + wire.branch.is_some() as u8
            + wire.default_branch as u8;

        if selected != 1 {
            return Err(D::Error::custom(
                "selector must include exactly one of commit, short_commit, branch, or default_branch",
            ));
        }

        if let Some(commit) = wire.commit {
            Ok(Self::Commit(commit))
        } else if let Some(commit) = wire.short_commit {
            Ok(Self::ShortCommit(commit))
        } else if let Some(branch) = wire.branch {
            Ok(Self::Branch(branch))
        } else {
            Ok(Self::DefaultBranch)
        }
    }
}

fn validate_branch_name(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitCacheError::Validation("branch name is empty".into()));
    }

    if value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.contains('\\')
        || value.contains("..")
        || value.starts_with("refs/")
        || value.ends_with(".lock")
    {
        return Err(GitCacheError::Validation(format!(
            "branch `{value}` is not a safe branch selector"
        )));
    }

    if value.bytes().any(|b| {
        b.is_ascii_control()
            || b == b'~'
            || b == b'^'
            || b == b':'
            || b == b'?'
            || b == b'*'
            || b == b'['
    }) {
        return Err(GitCacheError::Validation(format!(
            "branch `{value}` contains unsupported ref characters"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
