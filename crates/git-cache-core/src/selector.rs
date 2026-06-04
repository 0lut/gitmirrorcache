use crate::auth::ReachabilitySelector;
use crate::error::{GitCacheError, Result};
use crate::repo::{CommitSha, ShortCommitSha};
use serde::de::Error as _;
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
    CommitReachableFrom {
        commit: CommitSha,
        reachable_from: ReachabilitySelector,
    },
    ShortCommit(ShortCommitSha),
    Branch(BranchName),
    DefaultBranch,
}

impl Serialize for Selector {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Commit(commit) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("commit", commit)?;
                map.end()
            }
            Self::CommitReachableFrom {
                commit,
                reachable_from,
            } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("commit", commit)?;
                map.serialize_entry("reachable_from", reachable_from)?;
                map.end()
            }
            Self::ShortCommit(commit) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("short_commit", commit)?;
                map.end()
            }
            Self::Branch(branch) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("branch", branch)?;
                map.end()
            }
            Self::DefaultBranch => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("default_branch", &true)?;
                map.end()
            }
        }
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
            reachable_from: Option<ReachabilitySelector>,
            short_commit: Option<ShortCommitSha>,
            branch: Option<BranchName>,
            #[serde(default)]
            default_branch: bool,
        }

        let wire = WireSelector::deserialize(deserializer)?;

        // reachable_from is a modifier on commit, not a standalone selector
        let primary_selected = wire.commit.is_some() as u8
            + wire.short_commit.is_some() as u8
            + wire.branch.is_some() as u8
            + wire.default_branch as u8;

        if primary_selected != 1 {
            return Err(D::Error::custom(
                "selector must include exactly one of commit, short_commit, branch, or default_branch",
            ));
        }

        if wire.reachable_from.is_some() && wire.commit.is_none() {
            return Err(D::Error::custom(
                "reachable_from can only be used with commit selector",
            ));
        }

        if let Some(commit) = wire.commit {
            if let Some(reachable_from) = wire.reachable_from {
                Ok(Self::CommitReachableFrom {
                    commit,
                    reachable_from,
                })
            } else {
                Ok(Self::Commit(commit))
            }
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
mod tests {
    use super::*;

    #[test]
    fn selector_matches_wire_format() {
        let selector: Selector = serde_json::from_str(r#"{"branch":"main"}"#).unwrap();
        assert_eq!(
            selector,
            Selector::Branch(BranchName::parse("main").unwrap())
        );
        assert_eq!(
            serde_json::to_string(&Selector::DefaultBranch).unwrap(),
            r#"{"default_branch":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Selector::ShortCommit(
                ShortCommitSha::parse("abc123").unwrap()
            ))
            .unwrap(),
            r#"{"short_commit":"abc123"}"#
        );
    }

    #[test]
    fn selector_requires_one_field() {
        assert!(
            serde_json::from_str::<Selector>(r#"{"branch":"main","default_branch":true}"#).is_err()
        );
        assert!(serde_json::from_str::<Selector>(
            r#"{"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","short_commit":"aaaaaaa"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<Selector>(r#"{}"#).is_err());
    }

    #[test]
    fn branch_rejects_unsafe_refs() {
        for value in [
            "../main",
            "refs/heads/main",
            "main.lock",
            "feature//x",
            "bad:name",
        ] {
            assert!(BranchName::parse(value).is_err(), "{value}");
        }
    }

    // ── Additional BranchName correctness tests ──────────────────────

    #[test]
    fn branch_valid_simple_names() {
        assert!(BranchName::parse("main").is_ok());
        assert!(BranchName::parse("feature/foo").is_ok());
        assert!(BranchName::parse("release-1.0").is_ok());
    }

    #[test]
    fn branch_rejects_empty() {
        assert!(BranchName::parse("").is_err());
    }

    #[test]
    fn branch_rejects_leading_slash() {
        assert!(BranchName::parse("/main").is_err());
    }

    #[test]
    fn branch_rejects_trailing_slash() {
        assert!(BranchName::parse("feature/").is_err());
    }

    #[test]
    fn branch_rejects_dot_dot() {
        assert!(BranchName::parse("feature/..bad").is_err());
        assert!(BranchName::parse("..").is_err());
    }

    #[test]
    fn branch_rejects_backslash() {
        assert!(BranchName::parse("feature\\bar").is_err());
    }

    #[test]
    fn branch_rejects_control_chars() {
        assert!(BranchName::parse("main\x00").is_err());
        assert!(BranchName::parse("main\x07").is_err());
    }

    #[test]
    fn branch_rejects_tilde_caret_question_star_bracket() {
        assert!(BranchName::parse("main~1").is_err());
        assert!(BranchName::parse("main^1").is_err());
        assert!(BranchName::parse("main?").is_err());
        assert!(BranchName::parse("main*").is_err());
        assert!(BranchName::parse("main[0]").is_err());
    }

    #[test]
    fn branch_rejects_ending_dot_lock() {
        assert!(BranchName::parse("main.lock").is_err());
        assert!(BranchName::parse("feature/test.lock").is_err());
    }

    #[test]
    fn branch_rejects_starting_refs() {
        assert!(BranchName::parse("refs/heads/main").is_err());
        assert!(BranchName::parse("refs/tags/v1").is_err());
    }

    #[test]
    fn branch_ref_name_produces_full_ref() {
        let branch = BranchName::parse("main").unwrap();
        assert_eq!(branch.ref_name(), "refs/heads/main");
    }

    #[test]
    fn branch_serde_round_trip() {
        let branch = BranchName::parse("feature/test").unwrap();
        let json = serde_json::to_string(&branch).unwrap();
        assert_eq!(json, r#""feature/test""#);
        let parsed: BranchName = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, branch);
    }

    // ── Additional Selector deserialization tests ────────────────────

    #[test]
    fn selector_deserializes_branch() {
        let s: Selector = serde_json::from_str(r#"{"branch":"main"}"#).unwrap();
        assert_eq!(s, Selector::Branch(BranchName::parse("main").unwrap()));
    }

    #[test]
    fn selector_deserializes_commit() {
        let sha = "a".repeat(40);
        let json = format!(r#"{{"commit":"{sha}"}}"#);
        let s: Selector = serde_json::from_str(&json).unwrap();
        assert_eq!(s, Selector::Commit(CommitSha::parse(&sha).unwrap()));
    }

    #[test]
    fn selector_deserializes_short_commit() {
        let s: Selector = serde_json::from_str(r#"{"short_commit":"abcdef"}"#).unwrap();
        assert_eq!(
            s,
            Selector::ShortCommit(ShortCommitSha::parse("abcdef").unwrap())
        );
    }

    #[test]
    fn selector_deserializes_default_branch() {
        let s: Selector = serde_json::from_str(r#"{"default_branch":true}"#).unwrap();
        assert_eq!(s, Selector::DefaultBranch);
    }

    #[test]
    fn selector_rejects_zero_fields() {
        assert!(serde_json::from_str::<Selector>(r#"{}"#).is_err());
    }

    #[test]
    fn selector_rejects_multiple_fields() {
        assert!(
            serde_json::from_str::<Selector>(r#"{"branch":"main","default_branch":true}"#).is_err()
        );
    }

    #[test]
    fn selector_serialization_round_trips() {
        let cases = [
            Selector::Branch(BranchName::parse("main").unwrap()),
            Selector::DefaultBranch,
            Selector::Commit(CommitSha::parse("b".repeat(40)).unwrap()),
            Selector::ShortCommit(ShortCommitSha::parse("abcdef12").unwrap()),
        ];
        for selector in &cases {
            let json = serde_json::to_string(selector).unwrap();
            let parsed: Selector = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, selector);
        }
    }
}
