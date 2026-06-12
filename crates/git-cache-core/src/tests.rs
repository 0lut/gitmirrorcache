use super::*;

#[test]
fn materialize_source_uses_provider_neutral_public_label() {
    let serialized = serde_json::to_string(&MaterializeSource::UpstreamVerified).unwrap();
    assert_eq!(serialized, "\"upstream_verified\"");

    let parsed: MaterializeSource = serde_json::from_str("\"github_verified\"").unwrap();
    assert_eq!(parsed, MaterializeSource::UpstreamVerified);
}
