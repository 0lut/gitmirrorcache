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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        CommitManifest, GenerationId, GenerationManifest, RefManifest,
    };

    // ── SessionId tests ──────────────────────────────────────────────

    #[test]
    fn session_id_new_generates_unique_ids() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn session_id_parse_round_trips() {
        let id = SessionId::new();
        let s = id.to_string();
        let reparsed = SessionId::parse(&s).unwrap();
        assert_eq!(id, reparsed);
    }

    #[test]
    fn session_id_rejects_invalid_string() {
        assert!(SessionId::parse("not-a-uuid").is_err());
        assert!(SessionId::parse("").is_err());
    }

    #[test]
    fn session_id_synthetic_ref_format() {
        let id = SessionId::new();
        let ref_name = id.synthetic_ref();
        assert!(ref_name.starts_with("refs/cache/sessions/"));
        assert!(ref_name.len() > "refs/cache/sessions/".len());
    }

    #[test]
    fn session_id_serde_round_trip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    // ── GenerationId tests ───────────────────────────────────────────

    #[test]
    fn generation_id_new_generates_unique_ids() {
        let a = GenerationId::new();
        let b = GenerationId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn generation_id_serde_round_trip() {
        let id = GenerationId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: GenerationId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    // ── Manifest JSON round-trip tests ───────────────────────────────

    fn test_repo() -> RepoKey {
        RepoKey::parse("github.com/test/repo").unwrap()
    }

    fn test_commit() -> CommitSha {
        CommitSha::parse("a".repeat(40)).unwrap()
    }

    fn test_ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn generation_manifest_serde_round_trip() {
        let manifest = GenerationManifest {
            repo: test_repo(),
            generation: GenerationId::new(),
            bundle_key: "repos/test/bundle.bundle".into(),
            parent_generation: Some(GenerationId::new()),
            created_at: test_ts(),
            commits: vec![test_commit()],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: GenerationManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
    }

    #[test]
    fn generation_manifest_optional_parent_absent() {
        let manifest = GenerationManifest {
            repo: test_repo(),
            generation: GenerationId::new(),
            bundle_key: "repos/test/bundle.bundle".into(),
            parent_generation: None,
            created_at: test_ts(),
            commits: vec![],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: GenerationManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
        assert!(parsed.parent_generation.is_none());
    }

    #[test]
    fn commit_manifest_serde_round_trip() {
        let manifest = CommitManifest {
            repo: test_repo(),
            commit: test_commit(),
            generation: GenerationId::new(),
            complete: true,
            verified_at: test_ts(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: CommitManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
    }

    #[test]
    fn ref_manifest_serde_round_trip() {
        let manifest = RefManifest {
            repo: test_repo(),
            ref_name: "refs/heads/main".into(),
            commit: test_commit(),
            generation: GenerationId::new(),
            verified_at: test_ts(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: RefManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
    }

    #[test]
    fn session_manifest_serde_round_trip() {
        let id = SessionId::new();
        let manifest = SessionManifest {
            id,
            repo: test_repo(),
            commit: test_commit(),
            synthetic_ref: id.synthetic_ref(),
            created_at: test_ts(),
            expires_at: test_ts(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: SessionManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
    }
}
