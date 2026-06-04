use crate::error::{GitCacheError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Redaction-safe representation of upstream authorization credentials.
///
/// Tokens are never exposed through `Debug`, `Display`, or serialization.
/// Only the scheme is shown in redacted form.
#[derive(Clone)]
pub enum UpstreamAuth {
    Anonymous,
    Basic { raw: String },
}

impl UpstreamAuth {
    /// Parse an `Authorization` header value into an `UpstreamAuth`.
    ///
    /// Accepts only `Basic <credentials>` for this implementation.
    /// Rejects empty values, values with NUL or control characters,
    /// and non-Basic schemes.
    pub fn from_header_value(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Err(GitCacheError::Validation(
                "authorization header is empty".into(),
            ));
        }

        if value.bytes().any(|b| b == 0 || (b < 0x20 && b != b'\t')) {
            return Err(GitCacheError::Validation(
                "authorization header contains invalid characters".into(),
            ));
        }

        if let Some(credentials) = value.strip_prefix("Basic ") {
            let credentials = credentials.trim();
            if credentials.is_empty() {
                return Err(GitCacheError::Validation(
                    "Basic authorization credentials are empty".into(),
                ));
            }
            Ok(Self::Basic {
                raw: value.to_string(),
            })
        } else if let Some(credentials) = value.strip_prefix("basic ") {
            let credentials = credentials.trim();
            if credentials.is_empty() {
                return Err(GitCacheError::Validation(
                    "Basic authorization credentials are empty".into(),
                ));
            }
            // Normalize to canonical form
            Ok(Self::Basic {
                raw: format!("Basic {credentials}"),
            })
        } else {
            Err(GitCacheError::Validation(format!(
                "unsupported authorization scheme (expected Basic): {}",
                RedactedScheme(value)
            )))
        }
    }

    /// Returns the raw header value for injection into git commands.
    /// This must NEVER be logged, stored, or included in error messages.
    pub fn raw_header_value(&self) -> Option<&str> {
        match self {
            Self::Anonymous => None,
            Self::Basic { raw } => Some(raw),
        }
    }

    pub fn is_authenticated(&self) -> bool {
        !matches!(self, Self::Anonymous)
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self, Self::Anonymous)
    }
}

/// Display shows only the scheme, never the credentials.
impl fmt::Display for UpstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Anonymous => write!(f, "Anonymous"),
            Self::Basic { .. } => write!(f, "Basic <redacted>"),
        }
    }
}

/// Debug intentionally does not include raw credential data.
impl fmt::Debug for UpstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Anonymous => f.debug_struct("Anonymous").finish(),
            Self::Basic { .. } => f.debug_struct("Basic").field("raw", &"<redacted>").finish(),
        }
    }
}

/// Helper to show just the scheme prefix of an auth header in errors.
struct RedactedScheme<'a>(&'a str);

impl fmt::Display for RedactedScheme<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(space_pos) = self.0.find(' ') {
            write!(f, "{} <redacted>", &self.0[..space_pos])
        } else {
            write!(f, "<unrecognized-scheme>")
        }
    }
}

/// Mode for upstream authorization on API requests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamAuthorizationMode {
    #[default]
    Anonymous,
    Required,
}

/// Reachability context for exact-commit selectors in authenticated mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachabilitySelector {
    Branch(crate::BranchName),
    DefaultBranch,
}

/// Protection level for sessions.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionProtection {
    #[default]
    Public,
    BearerToken {
        token_hash: String,
        authorized_commits: Vec<crate::CommitSha>,
        authorized_refs: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_display() {
        let auth = UpstreamAuth::Anonymous;
        assert_eq!(format!("{auth}"), "Anonymous");
    }

    #[test]
    fn basic_display_redacts() {
        let auth = UpstreamAuth::from_header_value("Basic dXNlcjpwYXNz").unwrap();
        assert_eq!(format!("{auth}"), "Basic <redacted>");
    }

    #[test]
    fn basic_debug_redacts() {
        let auth = UpstreamAuth::from_header_value("Basic dXNlcjpwYXNz").unwrap();
        let debug = format!("{auth:?}");
        assert!(!debug.contains("dXNlcjpwYXNz"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn rejects_empty_header() {
        assert!(UpstreamAuth::from_header_value("").is_err());
        assert!(UpstreamAuth::from_header_value("   ").is_err());
    }

    #[test]
    fn rejects_nul_in_header() {
        assert!(UpstreamAuth::from_header_value("Basic abc\0def").is_err());
    }

    #[test]
    fn rejects_control_chars() {
        assert!(UpstreamAuth::from_header_value("Basic \x01abc").is_err());
    }

    #[test]
    fn rejects_empty_basic_credentials() {
        assert!(UpstreamAuth::from_header_value("Basic ").is_err());
        assert!(UpstreamAuth::from_header_value("Basic   ").is_err());
    }

    #[test]
    fn rejects_non_basic_scheme() {
        let err = UpstreamAuth::from_header_value("Bearer token123").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported authorization scheme"));
        assert!(!msg.contains("token123"));
    }

    #[test]
    fn accepts_valid_basic_auth() {
        let auth = UpstreamAuth::from_header_value("Basic dXNlcjpwYXNz").unwrap();
        assert!(auth.is_authenticated());
        assert!(!auth.is_anonymous());
        assert_eq!(auth.raw_header_value(), Some("Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn accepts_case_insensitive_basic() {
        let auth = UpstreamAuth::from_header_value("basic dXNlcjpwYXNz").unwrap();
        assert!(auth.is_authenticated());
        // Normalized to canonical form
        assert_eq!(auth.raw_header_value(), Some("Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn anonymous_is_not_authenticated() {
        assert!(!UpstreamAuth::Anonymous.is_authenticated());
        assert!(UpstreamAuth::Anonymous.is_anonymous());
        assert!(UpstreamAuth::Anonymous.raw_header_value().is_none());
    }

    #[test]
    fn upstream_authorization_mode_defaults_to_anonymous() {
        assert_eq!(
            UpstreamAuthorizationMode::default(),
            UpstreamAuthorizationMode::Anonymous
        );
    }

    #[test]
    fn upstream_authorization_mode_serde() {
        let json = serde_json::to_string(&UpstreamAuthorizationMode::Required).unwrap();
        assert_eq!(json, r#""required""#);
        let parsed: UpstreamAuthorizationMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, UpstreamAuthorizationMode::Required);
    }

    #[test]
    fn session_protection_public_serde() {
        let prot = SessionProtection::Public;
        let json = serde_json::to_string(&prot).unwrap();
        let parsed: SessionProtection = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, prot);
    }

    #[test]
    fn session_protection_bearer_serde() {
        let prot = SessionProtection::BearerToken {
            token_hash: "abc123".into(),
            authorized_commits: vec![],
            authorized_refs: vec!["refs/heads/main".into()],
        };
        let json = serde_json::to_string(&prot).unwrap();
        assert!(!json.contains("public"));
        let parsed: SessionProtection = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, prot);
    }
}
