use crate::{GitCacheError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub enum UpstreamAuth {
    Anonymous,
    Basic { raw: SecretString },
}

impl UpstreamAuth {
    pub fn parse_header(value: &str) -> Result<Self> {
        let value = value.trim();
        validate_auth_header(value)?;
        let Some((scheme, credential)) = value.split_once(char::is_whitespace) else {
            return Err(GitCacheError::Validation(
                "upstream authorization must use Basic authentication".into(),
            ));
        };
        if !scheme.eq_ignore_ascii_case("basic") || credential.trim().is_empty() {
            return Err(GitCacheError::Validation(
                "upstream authorization must use Basic authentication".into(),
            ));
        }
        Ok(Self::Basic {
            raw: SecretString::new(format!("Basic {}", credential.trim())),
        })
    }

    pub fn is_authenticated(&self) -> bool {
        matches!(self, Self::Basic { .. })
    }

    pub fn raw_header(&self) -> Option<&str> {
        match self {
            Self::Anonymous => None,
            Self::Basic { raw } => Some(raw.expose_secret()),
        }
    }

    pub fn redacted_header(&self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Basic { .. } => "Basic <redacted>",
        }
    }
}

impl Default for UpstreamAuth {
    fn default() -> Self {
        Self::Anonymous
    }
}

impl fmt::Debug for UpstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Anonymous => f.write_str("UpstreamAuth::Anonymous"),
            Self::Basic { .. } => f.write_str("UpstreamAuth::Basic(<redacted>)"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamAuthorizationMode {
    Anonymous,
    Required,
}

impl Default for UpstreamAuthorizationMode {
    fn default() -> Self {
        Self::Anonymous
    }
}

fn validate_auth_header(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitCacheError::Validation(
            "upstream authorization header is empty".into(),
        ));
    }
    if value
        .bytes()
        .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(GitCacheError::Validation(
            "upstream authorization header contains unsupported characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_auth_accepts_basic_and_redacts_debug() {
        let auth = UpstreamAuth::parse_header("Basic dXNlcjpnaHBfc2VjcmV0").unwrap();
        assert!(auth.is_authenticated());
        assert_eq!(auth.redacted_header(), "Basic <redacted>");
        assert!(!format!("{auth:?}").contains("dXNlcjpnaHBfc2VjcmV0"));
    }

    #[test]
    fn upstream_auth_rejects_non_basic() {
        assert!(UpstreamAuth::parse_header("Bearer token").is_err());
        assert!(UpstreamAuth::parse_header("").is_err());
    }

    #[test]
    fn upstream_auth_rejects_control_characters() {
        assert!(UpstreamAuth::parse_header("Basic abc\ndef").is_err());
        assert!(UpstreamAuth::parse_header("Basic abc\0def").is_err());
    }
}
