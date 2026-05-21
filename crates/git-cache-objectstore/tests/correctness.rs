//! Correctness edge-case tests for git-cache-objectstore.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{
    CommitManifest, CommitSha, GenerationId, GenerationManifest, RefManifest, RepoKey, SessionId,
    SessionManifest,
};
use git_cache_objectstore::{
    commit_manifest_key, generation_manifest_key, read_commit_manifest, read_generation_manifest,
    read_ref_manifest, read_session_manifest, ref_manifest_key, repo_generation_head_key,
    session_manifest_key, write_commit_manifest, write_commit_manifest_if_absent_or_matches,
    write_generation_manifest, write_ref_manifest, write_ref_manifest_if_absent_or_matches,
    write_session_manifest, write_session_manifest_if_absent_or_matches, LocalObjectStore,
    ObjectStore,
};
use tokio::fs;

fn temp_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "git-cache-objectstore-correctness-{}",
        uuid::Uuid::now_v7()
    ))
}

fn repo() -> RepoKey {
    RepoKey::parse("github.com/test/correctness").unwrap()
}

fn commit(byte: char) -> CommitSha {
    CommitSha::parse(byte.to_string().repeat(40)).unwrap()
}

fn ts(seconds: u32) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&format!("2026-05-21T00:00:{seconds:02}Z"))
        .unwrap()
        .with_timezone(&Utc)
}

// ── Object key validation (via validate_key) ────────────────────────────

#[test]
fn validate_key_rejects_empty_key() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("")).is_err());
}

#[test]
fn validate_key_rejects_leading_slash() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("/leading/slash")).is_err());
}

#[test]
fn validate_key_rejects_trailing_slash() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("trailing/slash/")).is_err());
}

#[test]
fn validate_key_rejects_backslash() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("back\\slash")).is_err());
}

#[test]
fn validate_key_rejects_nul_byte() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("has\0nul")).is_err());
}

#[test]
fn validate_key_rejects_dot_segments() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("path/./here")).is_err());
    assert!(rt.block_on(store.get("path/../escape")).is_err());
    assert!(rt.block_on(store.get("..")).is_err());
    assert!(rt.block_on(store.get(".")).is_err());
}

#[test]
fn validate_key_rejects_double_dots_segment() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(store.get("repos/../secret")).is_err());
}

#[test]
fn validate_key_accepts_normal_valid_keys() {
    let store = LocalObjectStore::new("/tmp/unused");
    let rt = tokio::runtime::Runtime::new().unwrap();
    // These should not error on validation (just return None for nonexistent)
    assert!(rt
        .block_on(store.get("repos/github.com/org/repo/manifest.json"))
        .is_ok());
    assert!(rt.block_on(store.get("simple.txt")).is_ok());
    assert!(rt.block_on(store.get("a/b/c")).is_ok());
}

// ── LocalObjectStore: get nonexistent key returns None ──────────────────

#[tokio::test]
async fn get_nonexistent_key_returns_none() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    let result = store.get("does/not/exist.json").await.unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&root).await;
}

// ── LocalObjectStore: put then get round-trip ───────────────────────────

#[tokio::test]
async fn put_then_get_round_trip() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    let data = Bytes::from("hello world");
    store.put("test/data.bin", data.clone()).await.unwrap();

    let retrieved = store.get("test/data.bin").await.unwrap().unwrap();
    assert_eq!(retrieved, data);

    let _ = fs::remove_dir_all(&root).await;
}

// ── LocalObjectStore: put overwrites existing value ─────────────────────

#[tokio::test]
async fn put_overwrites_existing_value() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    store
        .put("overwrite/key.json", Bytes::from("first"))
        .await
        .unwrap();
    store
        .put("overwrite/key.json", Bytes::from("second"))
        .await
        .unwrap();

    let result = store.get("overwrite/key.json").await.unwrap().unwrap();
    assert_eq!(result, Bytes::from("second"));

    let _ = fs::remove_dir_all(&root).await;
}

// ── LocalObjectStore: put_if_absent semantics ───────────────────────────

#[tokio::test]
async fn put_if_absent_first_write_succeeds() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    let created = store
        .put_if_absent("conditional/new.json", Bytes::from("data"))
        .await
        .unwrap();
    assert!(created);

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn put_if_absent_second_write_returns_false_preserves_original() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    store
        .put_if_absent("conditional/dup.json", Bytes::from("original"))
        .await
        .unwrap();
    let second = store
        .put_if_absent("conditional/dup.json", Bytes::from("overwrite"))
        .await
        .unwrap();
    assert!(!second);

    let data = store.get("conditional/dup.json").await.unwrap().unwrap();
    assert_eq!(data, Bytes::from("original"));

    let _ = fs::remove_dir_all(&root).await;
}

// ── LocalObjectStore: exists ────────────────────────────────────────────

#[tokio::test]
async fn exists_returns_false_for_nonexistent() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    assert!(!store.exists("ghost/key.json").await.unwrap());

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn exists_returns_true_after_put() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    store
        .put("exists/key.json", Bytes::from("y"))
        .await
        .unwrap();
    assert!(store.exists("exists/key.json").await.unwrap());

    let _ = fs::remove_dir_all(&root).await;
}

// ── LocalObjectStore: delete idempotency ────────────────────────────────

#[tokio::test]
async fn delete_existing_then_gone() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    store.put("del/item.json", Bytes::from("x")).await.unwrap();
    store.delete("del/item.json").await.unwrap();
    assert!(!store.exists("del/item.json").await.unwrap());
    assert!(store.get("del/item.json").await.unwrap().is_none());

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn delete_nonexistent_is_idempotent() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    // Should not error
    store.delete("never/existed.json").await.unwrap();
    store.delete("never/existed.json").await.unwrap();

    let _ = fs::remove_dir_all(&root).await;
}

// ── LocalObjectStore: list_prefix with nested keys ──────────────────────

#[tokio::test]
async fn list_prefix_with_nested_keys() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);

    store.put("ns/a.json", Bytes::from("a")).await.unwrap();
    store.put("ns/b.json", Bytes::from("b")).await.unwrap();
    store.put("ns/sub/c.json", Bytes::from("c")).await.unwrap();
    store
        .put("ns/sub/deep/d.json", Bytes::from("d"))
        .await
        .unwrap();
    store.put("other/e.json", Bytes::from("e")).await.unwrap();

    let mut keys = store.list_prefix("ns/").await.unwrap();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "ns/a.json",
            "ns/b.json",
            "ns/sub/c.json",
            "ns/sub/deep/d.json",
        ]
    );

    let sub_keys = store.list_prefix("ns/sub/").await.unwrap();
    assert_eq!(sub_keys.len(), 2);

    let empty = store.list_prefix("nonexistent/").await.unwrap();
    assert!(empty.is_empty());

    let _ = fs::remove_dir_all(&root).await;
}

// ── Manifest helpers: round-trip tests ──────────────────────────────────

#[tokio::test]
async fn generation_manifest_write_read_round_trip() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = GenerationManifest {
        repo: repo.clone(),
        generation: gen,
        bundle_key: format!("repos/{repo}/generations/{gen}/base.bundle"),
        parent_generation: None,
        created_at: ts(1),
        commits: vec![commit('a')],
    };

    write_generation_manifest(&store, &manifest).await.unwrap();
    let read = read_generation_manifest(&store, &repo, gen).await.unwrap();
    assert_eq!(read, Some(manifest));

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn commit_manifest_write_read_round_trip() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = CommitManifest {
        repo: repo.clone(),
        commit: commit('b'),
        generation: gen,
        complete: true,
        verified_at: ts(2),
    };

    write_commit_manifest(&store, &manifest).await.unwrap();
    let read = read_commit_manifest(&store, &repo, &commit('b'))
        .await
        .unwrap();
    assert_eq!(read, Some(manifest));

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn ref_manifest_write_read_round_trip() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = RefManifest {
        repo: repo.clone(),
        ref_name: "refs/heads/main".into(),
        commit: commit('c'),
        generation: gen,
        verified_at: ts(3),
    };

    write_ref_manifest(&store, &manifest).await.unwrap();
    let read = read_ref_manifest(&store, &repo, "refs/heads/main")
        .await
        .unwrap();
    assert_eq!(read, Some(manifest));

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn session_manifest_write_read_round_trip() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let sid = SessionId::new();

    let manifest = SessionManifest {
        id: sid,
        repo: repo.clone(),
        commit: commit('d'),
        synthetic_ref: sid.synthetic_ref(),
        created_at: ts(4),
        expires_at: ts(5),
    };

    write_session_manifest(&store, &manifest).await.unwrap();
    let read = read_session_manifest(&store, &repo, sid).await.unwrap();
    assert_eq!(read, Some(manifest));

    let _ = fs::remove_dir_all(&root).await;
}

// ── write_*_if_absent_or_matches tests ──────────────────────────────────

#[tokio::test]
async fn commit_manifest_if_absent_or_matches_first_write() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = CommitManifest {
        repo: repo.clone(),
        commit: commit('e'),
        generation: gen,
        complete: true,
        verified_at: ts(6),
    };

    let first = write_commit_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    assert!(first);

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn commit_manifest_if_absent_or_matches_identical_second() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = CommitManifest {
        repo: repo.clone(),
        commit: commit('e'),
        generation: gen,
        complete: true,
        verified_at: ts(6),
    };

    write_commit_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    let second = write_commit_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    assert!(!second);

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn commit_manifest_if_absent_or_matches_conflicting() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = CommitManifest {
        repo: repo.clone(),
        commit: commit('e'),
        generation: gen,
        complete: true,
        verified_at: ts(6),
    };

    write_commit_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();

    let mut conflicting = manifest;
    conflicting.complete = false;
    assert!(
        write_commit_manifest_if_absent_or_matches(&store, &conflicting)
            .await
            .is_err()
    );

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn session_manifest_if_absent_or_matches_first_write() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let sid = SessionId::new();

    let manifest = SessionManifest {
        id: sid,
        repo: repo.clone(),
        commit: commit('f'),
        synthetic_ref: sid.synthetic_ref(),
        created_at: ts(7),
        expires_at: ts(8),
    };

    let first = write_session_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    assert!(first);

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn session_manifest_if_absent_or_matches_identical_second() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let sid = SessionId::new();

    let manifest = SessionManifest {
        id: sid,
        repo: repo.clone(),
        commit: commit('f'),
        synthetic_ref: sid.synthetic_ref(),
        created_at: ts(7),
        expires_at: ts(8),
    };

    write_session_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    let second = write_session_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    assert!(!second);

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn session_manifest_if_absent_or_matches_conflicting() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let sid = SessionId::new();

    let manifest = SessionManifest {
        id: sid,
        repo: repo.clone(),
        commit: commit('f'),
        synthetic_ref: sid.synthetic_ref(),
        created_at: ts(7),
        expires_at: ts(8),
    };

    write_session_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();

    let mut conflicting = manifest;
    conflicting.commit = commit('a');
    assert!(
        write_session_manifest_if_absent_or_matches(&store, &conflicting)
            .await
            .is_err()
    );

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn ref_manifest_if_absent_or_matches_first_succeeds() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = RefManifest {
        repo: repo.clone(),
        ref_name: "refs/heads/develop".into(),
        commit: commit('a'),
        generation: gen,
        verified_at: ts(1),
    };

    assert!(write_ref_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap());

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn ref_manifest_if_absent_or_matches_identical_returns_false() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();
    let gen = GenerationId::new();

    let manifest = RefManifest {
        repo: repo.clone(),
        ref_name: "refs/heads/develop".into(),
        commit: commit('a'),
        generation: gen,
        verified_at: ts(1),
    };

    write_ref_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap();
    assert!(!write_ref_manifest_if_absent_or_matches(&store, &manifest)
        .await
        .unwrap());

    let _ = fs::remove_dir_all(&root).await;
}

// ── Key generation functions produce expected paths ──────────────────────

#[test]
fn generation_manifest_key_format() {
    let repo = repo();
    let gen = GenerationId::new();
    let key = generation_manifest_key(&repo, gen);
    assert!(key.starts_with("repos/github.com/test/correctness/generations/"));
    assert!(key.ends_with("/manifest.json"));
}

#[test]
fn commit_manifest_key_format() {
    let repo = repo();
    let sha = commit('a');
    let key = commit_manifest_key(&repo, &sha);
    let expected = format!(
        "repos/github.com/test/correctness/manifests/commits/aa/{}.json",
        "a".repeat(40)
    );
    assert_eq!(key, expected);
}

#[test]
fn ref_manifest_key_format() {
    let repo = repo();
    let key = ref_manifest_key(&repo, "refs/heads/main").unwrap();
    assert_eq!(
        key,
        "repos/github.com/test/correctness/manifests/refs/heads/main.json"
    );
}

#[test]
fn ref_manifest_key_encodes_slashes_in_branch() {
    let repo = repo();
    let key = ref_manifest_key(&repo, "refs/heads/feature/deep").unwrap();
    assert_eq!(
        key,
        "repos/github.com/test/correctness/manifests/refs/heads/feature%2Fdeep.json"
    );
}

#[test]
fn session_manifest_key_format() {
    let repo = repo();
    let sid = SessionId::new();
    let key = session_manifest_key(&repo, sid);
    assert_eq!(
        key,
        format!("repos/github.com/test/correctness/manifests/sessions/{sid}.json")
    );
}

#[test]
fn repo_generation_head_key_format() {
    let repo = repo();
    let key = repo_generation_head_key(&repo);
    assert_eq!(
        key,
        "repos/github.com/test/correctness/manifests/generation-head.json"
    );
}

// ── Reading nonexistent manifests returns None ──────────────────────────

#[tokio::test]
async fn read_nonexistent_generation_manifest_returns_none() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let result = read_generation_manifest(&store, &repo, GenerationId::new())
        .await
        .unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn read_nonexistent_commit_manifest_returns_none() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let result = read_commit_manifest(&store, &repo, &commit('f'))
        .await
        .unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn read_nonexistent_session_manifest_returns_none() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let result = read_session_manifest(&store, &repo, SessionId::new())
        .await
        .unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&root).await;
}

#[tokio::test]
async fn read_nonexistent_ref_manifest_returns_none() {
    let root = temp_root();
    let store = LocalObjectStore::new(&root);
    let repo = repo();

    let result = read_ref_manifest(&store, &repo, "refs/heads/nonexistent")
        .await
        .unwrap();
    assert!(result.is_none());

    let _ = fs::remove_dir_all(&root).await;
}
