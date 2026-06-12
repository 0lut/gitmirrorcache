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
