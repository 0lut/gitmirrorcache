//! Git wire protocol v2 support for the direct Git endpoint.
//!
//! The direct Git GET path synthesizes ref advertisements from upstream
//! `ls-remote` output instead of running `git upload-pack --advertise-refs`,
//! so protocol v2 requests are answered here: the capability advertisement
//! and the `ls-refs` / `bundle-uri` commands are synthesized from the same
//! upstream comparison, while `fetch` commands run through the normal
//! read-through path with `GIT_PROTOCOL=version=2` pinned on the spawned
//! `git upload-pack`.

use super::UpstreamRefComparison;
use git_cache_core::{GenerationManifest, GitCacheError, Result as CoreResult};

/// Returns true when the request's `Git-Protocol` header opts into
/// protocol v2 (`version=2`, possibly alongside other key/value tokens).
pub fn wants_protocol_v2(git_protocol_header: Option<&str>) -> bool {
    git_protocol_header
        .map(|value| value.split(':').any(|token| token.trim() == "version=2"))
        .unwrap_or(false)
}

/// The protocol-v2 command carried by a stateless-rpc upload-pack body, if
/// the body is a v2 request (`command=<name>` first packet).
pub fn protocol_v2_command(body: &[u8]) -> Option<String> {
    let first = first_pkt_line(body)?;
    let rest = first.strip_prefix("command=")?;
    Some(rest.trim_end_matches('\n').to_string())
}

fn first_pkt_line(body: &[u8]) -> Option<String> {
    if body.len() < 4 {
        return None;
    }
    let pkt_len = usize::from_str_radix(std::str::from_utf8(&body[..4]).ok()?, 16).ok()?;
    if pkt_len < 4 || pkt_len > body.len() {
        return None;
    }
    String::from_utf8(body[4..pkt_len].to_vec()).ok()
}

/// Synthesize the protocol-v2 capability advertisement, equivalent to
/// `GIT_PROTOCOL=version=2 git upload-pack --advertise-refs`.
pub fn synthesize_capability_advertisement(advertise_bundle_uri: bool) -> Vec<u8> {
    let mut out = Vec::new();
    pkt_line(&mut out, "version 2\n");
    pkt_line(&mut out, "agent=git-cache/1.0\n");
    pkt_line(&mut out, "ls-refs\n");
    pkt_line(&mut out, "fetch=shallow filter\n");
    pkt_line(&mut out, "server-option\n");
    pkt_line(&mut out, "object-format=sha1\n");
    if advertise_bundle_uri {
        pkt_line(&mut out, "bundle-uri\n");
    }
    out.extend_from_slice(b"0000");
    out
}

#[derive(Debug, Default)]
pub struct LsRefsArgs {
    pub ref_prefixes: Vec<String>,
    pub symrefs: bool,
}

/// Parse the arguments of a protocol-v2 `ls-refs` command body.
pub fn parse_ls_refs_args(body: &[u8]) -> LsRefsArgs {
    let mut args = LsRefsArgs::default();
    visit_pkt_lines(body, |line| {
        let line = line.trim_end_matches('\n');
        if let Some(prefix) = line.strip_prefix("ref-prefix ") {
            if !prefix.is_empty() {
                args.ref_prefixes.push(prefix.to_string());
            }
        } else if line == "symrefs" {
            args.symrefs = true;
        }
    });
    args
}

/// Synthesize a protocol-v2 `ls-refs` response from upstream ref data,
/// matching what `git upload-pack` would return for the advertised refs.
pub fn synthesize_ls_refs_response(
    comparison: &UpstreamRefComparison,
    args: &LsRefsArgs,
) -> Vec<u8> {
    let mut out = Vec::new();

    let resolved_default = comparison.default_branch.as_ref().and_then(|branch| {
        comparison
            .all_upstream
            .get(branch)
            .map(|sha| (branch.as_str(), sha.as_str()))
    });

    let matches_prefix = |name: &str| {
        args.ref_prefixes.is_empty()
            || args
                .ref_prefixes
                .iter()
                .any(|p| name.starts_with(p.as_str()))
    };

    if let Some((branch, sha)) = resolved_default {
        if matches_prefix("HEAD") {
            let line = if args.symrefs {
                format!("{sha} HEAD symref-target:refs/heads/{branch}\n")
            } else {
                format!("{sha} HEAD\n")
            };
            pkt_line(&mut out, &line);
        }
    }

    let mut refs: Vec<(&String, &String)> = comparison.all_upstream.iter().collect();
    refs.sort_by_key(|(name, _)| name.as_str());
    for (name, sha) in refs {
        let ref_name = format!("refs/heads/{name}");
        if matches_prefix(&ref_name) {
            pkt_line(&mut out, &format!("{sha} {ref_name}\n"));
        }
    }

    out.extend_from_slice(b"0000");
    out
}

/// Synthesize a protocol-v2 `bundle-uri` response advertising generation
/// bundles as `{base_url}/{bundle_key}`, oldest (chain root) first.
pub fn synthesize_bundle_uri_response(
    base_url: &str,
    chain: &[GenerationManifest],
) -> CoreResult<Vec<u8>> {
    let base_url = base_url.trim_end_matches('/');
    if base_url.is_empty()
        || base_url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(GitCacheError::Validation(
            "bundle_uri_base_url must be a non-empty URL without whitespace".into(),
        ));
    }

    let mut out = Vec::new();
    pkt_line(&mut out, "bundle.version=1\n");
    pkt_line(&mut out, "bundle.mode=all\n");
    for manifest in chain {
        if manifest
            .bundle_key
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(GitCacheError::Validation(format!(
                "bundle key `{}` is not advertisable as a bundle URI",
                manifest.bundle_key
            )));
        }
        let id = format!("g{}", manifest.generation.0.simple());
        pkt_line(
            &mut out,
            &format!("bundle.{id}.uri={base_url}/{}\n", manifest.bundle_key),
        );
    }
    out.extend_from_slice(b"0000");
    Ok(out)
}

fn visit_pkt_lines(body: &[u8], mut visit: impl FnMut(&str)) {
    let mut offset = 0;
    while offset + 4 <= body.len() {
        let hex = match std::str::from_utf8(&body[offset..offset + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let pkt_len = match usize::from_str_radix(hex, 16) {
            Ok(l) => l,
            Err(_) => break,
        };
        if pkt_len < 4 {
            offset += 4;
            continue;
        }
        if offset + pkt_len > body.len() {
            break;
        }
        if let Ok(line) = std::str::from_utf8(&body[offset + 4..offset + pkt_len]) {
            visit(line);
        }
        offset += pkt_len;
    }
}

fn pkt_line(out: &mut Vec<u8>, data: &str) {
    let len = 4 + data.len();
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(data.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_cache_core::{GenerationId, RepoKey};
    use std::collections::HashMap;

    fn pkt(data: &str) -> Vec<u8> {
        let mut out = Vec::new();
        pkt_line(&mut out, data);
        out
    }

    fn comparison() -> UpstreamRefComparison {
        let mut all_upstream = HashMap::new();
        all_upstream.insert("main".to_string(), "a".repeat(40));
        all_upstream.insert("dev".to_string(), "b".repeat(40));
        UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream,
        }
    }

    #[test]
    fn wants_protocol_v2_parses_header_tokens() {
        assert!(wants_protocol_v2(Some("version=2")));
        assert!(wants_protocol_v2(Some("key=value:version=2")));
        assert!(!wants_protocol_v2(Some("version=1")));
        assert!(!wants_protocol_v2(None));
    }

    #[test]
    fn protocol_v2_command_reads_first_pkt() {
        let mut body = pkt("command=ls-refs\n");
        body.extend_from_slice(b"0000");
        assert_eq!(protocol_v2_command(&body).as_deref(), Some("ls-refs"));
        assert_eq!(protocol_v2_command(b"0032want aaaa\n"), None);
        assert_eq!(protocol_v2_command(b""), None);
    }

    #[test]
    fn capability_advertisement_gates_bundle_uri() {
        let without = synthesize_capability_advertisement(false);
        let with = synthesize_capability_advertisement(true);
        let without = String::from_utf8(without).unwrap();
        let with = String::from_utf8(with).unwrap();
        assert!(without.contains("version 2"));
        assert!(!without.contains("bundle-uri"));
        assert!(with.contains("bundle-uri"));
        assert!(with.ends_with("0000"));
    }

    #[test]
    fn ls_refs_parses_args_and_synthesizes_response() {
        let mut body = pkt("command=ls-refs\n");
        body.extend_from_slice(b"0001");
        body.extend_from_slice(&pkt("symrefs\n"));
        body.extend_from_slice(&pkt("ref-prefix HEAD\n"));
        body.extend_from_slice(&pkt("ref-prefix refs/heads/\n"));
        body.extend_from_slice(b"0000");
        let args = parse_ls_refs_args(&body);
        assert!(args.symrefs);
        assert_eq!(
            args.ref_prefixes,
            vec!["HEAD".to_string(), "refs/heads/".to_string()]
        );

        let output = synthesize_ls_refs_response(&comparison(), &args);
        let output = String::from_utf8(output).unwrap();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        assert!(output.contains(&format!("{a} HEAD symref-target:refs/heads/main")));
        assert!(output.contains(&format!("{b} refs/heads/dev")));
        assert!(output.contains(&format!("{a} refs/heads/main")));
        assert!(output.ends_with("0000"));
    }

    #[test]
    fn ls_refs_prefix_filters_refs() {
        let mut args = LsRefsArgs::default();
        args.ref_prefixes.push("refs/heads/main".to_string());
        let output = synthesize_ls_refs_response(&comparison(), &args);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("refs/heads/main"));
        assert!(!output.contains("refs/heads/dev"));
        assert!(!output.contains("HEAD"));
    }

    #[test]
    fn bundle_uri_response_lists_chain_root_first() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let root = GenerationManifest {
            repo: repo.clone(),
            generation: GenerationId::new(),
            bundle_key: "bundles/root.bundle".into(),
            parent_generation: None,
            created_at: chrono::Utc::now(),
            commits: Vec::new(),
        };
        let child = GenerationManifest {
            repo,
            generation: GenerationId::new(),
            bundle_key: "bundles/child.bundle".into(),
            parent_generation: Some(root.generation),
            created_at: chrono::Utc::now(),
            commits: Vec::new(),
        };
        let output =
            synthesize_bundle_uri_response("https://cdn.example/base/", &[root, child]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("bundle.version=1"));
        assert!(output.contains("bundle.mode=all"));
        let root_pos = output.find("root.bundle").unwrap();
        let child_pos = output.find("child.bundle").unwrap();
        assert!(root_pos < child_pos);
        assert!(output.contains("https://cdn.example/base/bundles/root.bundle"));

        assert!(synthesize_bundle_uri_response("", &[]).is_err());
        assert!(synthesize_bundle_uri_response("http://x y", &[]).is_err());
    }
}
