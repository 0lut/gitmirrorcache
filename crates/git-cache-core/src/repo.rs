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
mod tests {
    use super::*;

    #[test]
    fn repo_key_rejects_traversal() {
        assert!(RepoKey::parse("../org/repo").is_err());
        assert!(RepoKey::parse("github.com/org/../repo").is_err());
        assert!(RepoKey::parse("github.com/org\\repo").is_err());
    }

    #[test]
    fn commit_sha_requires_full_hex() {
        assert!(CommitSha::parse("abc123").is_err());
        assert!(CommitSha::parse("a".repeat(40)).is_ok());
        assert!(CommitSha::parse("A".repeat(40))
            .unwrap()
            .as_str()
            .chars()
            .all(|c| c == 'a'));
    }

    #[test]
    fn short_commit_sha_accepts_abbreviated_hex_only() {
        assert!(ShortCommitSha::parse("abc").is_err());
        assert!(ShortCommitSha::parse("abc123").is_ok());
        assert!(ShortCommitSha::parse("A".repeat(12))
            .unwrap()
            .as_str()
            .chars()
            .all(|c| c == 'a'));
        assert!(ShortCommitSha::parse("g".repeat(8)).is_err());
        assert!(ShortCommitSha::parse("a".repeat(40)).is_err());
        assert!(ShortCommitSha::parse("a".repeat(64)).is_err());
    }

    // ── Additional RepoKey correctness tests ─────────────────────────

    #[test]
    fn repo_key_valid_standard() {
        let key = RepoKey::parse("github.com/org/repo").unwrap();
        assert_eq!(key.host(), "github.com");
        assert_eq!(key.owner(), "org");
        assert_eq!(key.name(), "repo");
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    #[test]
    fn repo_key_valid_with_dots_dashes_underscores() {
        assert!(RepoKey::parse("my-host.io/my_org/my-repo").is_ok());
        assert!(RepoKey::parse("a.b.c/d-e/f_g").is_ok());
    }

    #[test]
    fn repo_key_rejects_empty() {
        assert!(RepoKey::parse("").is_err());
    }

    #[test]
    fn repo_key_rejects_too_few_segments() {
        assert!(RepoKey::parse("github.com/org").is_err());
        assert!(RepoKey::parse("github.com").is_err());
    }

    #[test]
    fn repo_key_rejects_too_many_segments() {
        assert!(RepoKey::parse("github.com/org/repo/extra").is_err());
    }

    #[test]
    fn repo_key_rejects_special_chars() {
        assert!(RepoKey::parse("github.com/org/rep@o").is_err());
        assert!(RepoKey::parse("github.com/org/rep o").is_err());
    }

    #[test]
    fn repo_key_rejects_dot_dot_segments() {
        assert!(RepoKey::parse("github.com/../repo").is_err());
        assert!(RepoKey::parse("github.com/org/..").is_err());
    }

    #[test]
    fn repo_key_rejects_single_dot_segments() {
        assert!(RepoKey::parse("github.com/./repo").is_err());
        assert!(RepoKey::parse("./org/repo").is_err());
    }

    #[test]
    fn repo_key_rejects_leading_slash() {
        assert!(RepoKey::parse("/github.com/org/repo").is_err());
    }

    #[test]
    fn repo_key_rejects_trailing_slash() {
        assert!(RepoKey::parse("github.com/org/repo/").is_err());
    }

    #[test]
    fn repo_key_rejects_backslash() {
        assert!(RepoKey::parse("github.com\\org/repo").is_err());
    }

    #[test]
    fn repo_key_rejects_nul_byte() {
        assert!(RepoKey::parse("github.com/org/re\0po").is_err());
    }

    #[test]
    fn repo_key_display_round_trips() {
        let key = RepoKey::parse("github.com/org/repo").unwrap();
        let reparsed = RepoKey::parse(key.to_string()).unwrap();
        assert_eq!(key, reparsed);
    }

    #[test]
    fn repo_key_local_bare_path() {
        let key = RepoKey::parse("github.com/org/repo").unwrap();
        assert_eq!(key.local_bare_path(), "github.com/org/repo.git");
    }

    #[test]
    fn repo_key_serde_round_trip() {
        let key = RepoKey::parse("github.com/org/repo").unwrap();
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, r#""github.com/org/repo""#);
        let parsed: RepoKey = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn repo_key_serde_rejects_invalid() {
        assert!(serde_json::from_str::<RepoKey>(r#""invalid""#).is_err());
    }

    // ── Additional CommitSha correctness tests ───────────────────────

    #[test]
    fn commit_sha_valid_40_hex() {
        let sha = CommitSha::parse("a".repeat(40)).unwrap();
        assert_eq!(sha.as_str(), "a".repeat(40));
    }

    #[test]
    fn commit_sha_mixed_case_normalizes_to_lowercase() {
        let mixed = "aAbBcCdDeEfF"
            .repeat(4)
            .chars()
            .take(40)
            .collect::<String>();
        assert_eq!(mixed.len(), 40);
        let sha = CommitSha::parse(&mixed).unwrap();
        assert!(sha.as_str().chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn commit_sha_rejects_too_short() {
        assert!(CommitSha::parse("a".repeat(39)).is_err());
    }

    #[test]
    fn commit_sha_rejects_too_long() {
        assert!(CommitSha::parse("a".repeat(41)).is_err());
    }

    #[test]
    fn commit_sha_rejects_non_hex() {
        let mut bad = "a".repeat(39);
        bad.push('g');
        assert!(CommitSha::parse(&bad).is_err());
    }

    #[test]
    fn commit_sha_rejects_empty() {
        assert!(CommitSha::parse("").is_err());
    }

    #[test]
    fn commit_sha_display_round_trips() {
        let sha = CommitSha::parse("b".repeat(40)).unwrap();
        let reparsed = CommitSha::parse(sha.to_string()).unwrap();
        assert_eq!(sha, reparsed);
    }

    // ── Additional ShortCommitSha correctness tests ──────────────────

    #[test]
    fn short_commit_sha_valid_4_chars() {
        assert!(ShortCommitSha::parse("abcd").is_ok());
    }

    #[test]
    fn short_commit_sha_valid_39_chars() {
        assert!(ShortCommitSha::parse("a".repeat(39)).is_ok());
    }

    #[test]
    fn short_commit_sha_rejects_less_than_4() {
        assert!(ShortCommitSha::parse("abc").is_err());
        assert!(ShortCommitSha::parse("ab").is_err());
        assert!(ShortCommitSha::parse("a").is_err());
    }

    #[test]
    fn short_commit_sha_rejects_full_40_chars() {
        assert!(ShortCommitSha::parse("a".repeat(40)).is_err());
    }

    #[test]
    fn short_commit_sha_rejects_non_hex() {
        assert!(ShortCommitSha::parse("ghijklmn").is_err());
    }

    #[test]
    fn short_commit_sha_normalizes_to_lowercase() {
        let sha = ShortCommitSha::parse("ABCDEF12").unwrap();
        assert_eq!(sha.as_str(), "abcdef12");
    }

    #[test]
    fn short_commit_sha_rejects_empty() {
        assert!(ShortCommitSha::parse("").is_err());
    }
}
